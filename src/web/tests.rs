use super::*;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use std::sync::atomic::Ordering;
use tower::ServiceExt;

#[path = "mock_school.rs"]
mod mock_school;

struct Fixture {
    directory: std::path::PathBuf,
    config: AppConfig,
    state: Arc<WebState>,
}

impl Fixture {
    async fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("upm-web-{}", random_secret().unwrap()));
        std::fs::create_dir_all(&directory).unwrap();
        let config: AppConfig = serde_json::from_value(serde_json::json!({
            "database_url": format!("sqlite://{}", directory.join("monitor.db").display()),
            "cookie_file": directory.join("cookies.json"),
            "cookie_encryption_key": "test-cookie-secret",
            "interval_seconds": 1
        }))
        .unwrap();
        let db = DbService::new(config.database_url.clone()).await.unwrap();
        db.init().await.unwrap();
        let state = WebState::new(&config, "a".repeat(64), db).unwrap();
        Self {
            directory,
            config,
            state,
        }
    }

    async fn use_school(&self, school: &mock_school::School) {
        self.state.session.lock().await.api.client =
            uestc_client::UestcClient::with_encrypted_cookie_file_and_builder(
                &self.config.cookie_file,
                b"test-cookie-secret",
                school.builder(),
            );
    }

    async fn request(&self, path: &str, body: Option<serde_json::Value>) -> Response {
        let mut request = Request::builder().uri(path).header(
            header::AUTHORIZATION,
            format!("Bearer {}", self.state.token),
        );
        let body = if let Some(body) = body {
            request = request
                .method("POST")
                .header(header::CONTENT_TYPE, "application/json");
            Body::from(body.to_string())
        } else {
            Body::empty()
        };
        router(self.state.clone())
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap()
    }

    async fn phase(&self, expected: &str) {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if self.state.status.lock().unwrap().phase == expected {
                    // Wait for the operation to release the client before the next request.
                    if self.state.session.try_lock().is_ok() || expected == "qr_pending" {
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("expected authentication phase");
    }

    async fn password_login(&self) {
        assert_eq!(self.request("/api/login", Some(serde_json::json!({"login_type":"password","username":"alice","password":"test-password"}))).await.status(), StatusCode::ACCEPTED);
        self.phase("reauth_required").await;
    }

    /// 在临时目录写入 config.toml 并让状态机指向它（生产环境指向工作目录下的文件）。
    fn use_config_file(&mut self, content: &str) -> std::path::PathBuf {
        let path = self.directory.join("config.toml");
        std::fs::write(&path, content).unwrap();
        Arc::get_mut(&mut self.state).unwrap().config_file = Some(path.clone());
        path
    }

    fn config_file_content(&self, interval: u64) -> String {
        format!(
            "# 测试配置\ninterval_seconds = {interval}\ndatabase_url = \"sqlite://{}\"\ncookie_file = \"{}\"\n",
            self.directory.join("monitor.db").display(),
            self.config.cookie_file
        )
    }

    async fn config_json(&self, body: serde_json::Value) -> serde_json::Value {
        let response = self.request("/api/config", Some(body)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn finish_code(&self) {
        assert_eq!(
            self.request(
                "/api/reauth",
                Some(serde_json::json!({"method":3,"code":"123456","trust_device":true}))
            )
            .await
            .status(),
            StatusCode::ACCEPTED
        );
        self.phase("monitoring").await;
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[tokio::test]
async fn portal_session_check_uses_current_user_info_endpoint() {
    let school = mock_school::School::new();
    let fixture = Fixture::new().await;
    fixture.use_school(&school).await;
    school.stage.store(2, Ordering::SeqCst);
    let session = fixture.state.session.lock().await;
    // The retired language endpoint returns 404, even with a valid CAS session.
    assert!(session.api.initialize_business_session().await.is_ok());
}

#[tokio::test]
async fn empty_credentials_serve_a_live_page_and_protected_waiting_status() {
    let fixture = Fixture::new().await;
    let app = router(fixture.state.clone());
    for (path, code) in [
        ("/", StatusCode::OK),
        ("/healthz", StatusCode::OK),
        ("/app.js", StatusCode::OK),
        ("/api/status", StatusCode::UNAUTHORIZED),
        ("/api/history", StatusCode::UNAUTHORIZED),
        ("/api/config", StatusCode::UNAUTHORIZED),
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), code);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
    // An unequal token length must produce 401 rather than panic in constant-time comparison.
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .header(header::AUTHORIZATION, "Bearer short")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = fixture.request("/api/status", None).await;
    let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["phase"], "awaiting_login");
    assert!(!String::from_utf8_lossy(&body).contains(&fixture.state.token));
}

#[tokio::test]
async fn history_is_bounded_newest_first_persistent_and_available_during_authentication() {
    let fixture = Fixture::new().await;
    let response = fixture.request("/api/history", None).await;
    let records: Vec<serde_json::Value> =
        serde_json::from_slice(&to_bytes(response.into_body(), 16384).await.unwrap()).unwrap();
    assert!(records.is_empty());
    for money in 0..15 {
        let data = crate::api::PowerInfo {
            code: 0,
            message: "ok".into(),
            remaining_money: money as f64,
            remaining_energy: 23.5,
            room_display_name: "宿舍 A".into(),
            meter_room_id: "meter".into(),
            room_id: "room".into(),
            building_id: "building".into(),
            campus_id: "campus".into(),
            room_number: "1".into(),
        };
        fixture.state.db.save_data(&data).await.unwrap();
    }
    let recovered_db = DbService::new(fixture.config.database_url.clone())
        .await
        .unwrap();
    let recovered = WebState::new(&fixture.config, "a".repeat(64), recovered_db).unwrap();
    // QR polling holds the session lock; it must not block history reads.
    let _session = recovered.session.lock().await;
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        router(recovered.clone()).oneshot(
            Request::builder()
                .uri("/api/history")
                .header(header::AUTHORIZATION, format!("Bearer {}", "a".repeat(64)))
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let records: Vec<serde_json::Value> =
        serde_json::from_slice(&to_bytes(response.into_body(), 16384).await.unwrap()).unwrap();
    assert_eq!(records.len(), 10);
    assert_eq!(records[0]["remaining_money"], 14.0);
    assert_eq!(records[9]["remaining_money"], 5.0);
    assert_eq!(records[0]["remaining_energy"], 23.5);
    assert_eq!(records[0]["room_display_name"], "宿舍 A");
    assert!(
        chrono::DateTime::parse_from_rfc3339(records[0]["created_at"].as_str().unwrap()).is_ok()
    );
    assert!(records[0].get("meter_room_id").is_none());
}

#[tokio::test]
async fn manual_refresh_is_protected_coalesced_immediate_and_preserves_history_on_failure() {
    let school = mock_school::School::new();
    let mut fixture = Fixture::new().await;
    fixture.use_school(&school).await;
    fixture.config.interval_seconds = 3600;
    let response = router(fixture.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/refresh")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        fixture
            .request("/api/refresh", Some(serde_json::json!({})))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    school.stage.store(2, Ordering::SeqCst);
    fixture.state.session.lock().await.ready = true;
    fixture.state.update("monitoring", "ready");
    let scenario = async {
        let wait_finished = || async {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let status = fixture.state.status.lock().unwrap().clone();
                    if !status.refreshing && status.reading.is_some() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
        };
        wait_finished().await;
        assert_eq!(fixture.state.db.recent_records(10).await.unwrap().len(), 1);
        assert_eq!(
            fixture
                .request("/api/refresh", Some(serde_json::json!({})))
                .await
                .status(),
            StatusCode::ACCEPTED
        );
        assert!(fixture.state.status.lock().unwrap().refreshing);
        assert_eq!(
            fixture
                .request("/api/refresh", Some(serde_json::json!({})))
                .await
                .status(),
            StatusCode::CONFLICT
        );
        wait_finished().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(fixture.state.db.recent_records(10).await.unwrap().len(), 2);
        school.stage.store(3, Ordering::SeqCst);
        assert_eq!(
            fixture
                .request("/api/refresh", Some(serde_json::json!({})))
                .await
                .status(),
            StatusCode::ACCEPTED
        );
        wait_finished().await;
        assert_eq!(fixture.state.db.recent_records(10).await.unwrap().len(), 2);
        assert!(
            fixture
                .state
                .status
                .lock()
                .unwrap()
                .message
                .starts_with("本次采集失败")
        );
        assert!(fixture.state.session.lock().await.ready);
    };
    tokio::select! {
        _ = monitor(fixture.state.clone(), fixture.state.db.clone(), fixture.config.clone()) => panic!("monitor exited"),
        _ = scenario => {},
    }
}

#[tokio::test]
async fn requests_validate_origin_input_and_concurrent_operation_without_network() {
    let fixture = Fixture::new().await;
    let request = Request::builder()
        .method("POST")
        .uri("/api/login")
        .header(header::HOST, "localhost:8080")
        .header(header::ORIGIN, "https://evil.example")
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", fixture.state.token),
        )
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(
        router(fixture.state.clone())
            .oneshot(request)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        fixture
            .request(
                "/api/login",
                Some(serde_json::json!({"login_type":"password"}))
            )
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fixture
            .request(
                "/api/reauth",
                Some(serde_json::json!({"method":3,"code":"123"}))
            )
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let guard = fixture.state.session.lock().await;
    assert_eq!(
        fixture
            .request(
                "/api/login",
                Some(serde_json::json!({"login_type":"wechat"}))
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        fixture.request("/api/status", None).await.status(),
        StatusCode::OK
    );
    drop(guard);
}

#[tokio::test]
async fn password_reauth_send_throttle_wrong_code_and_persisted_recovery() {
    let school = mock_school::School::new();
    let fixture = Fixture::new().await;
    fixture.use_school(&school).await;
    fixture.password_login().await;
    assert_eq!(fixture.state.status.lock().unwrap().methods.len(), 3);
    for _ in 0..2 {
        assert_eq!(
            fixture
                .request(
                    "/api/reauth",
                    Some(serde_json::json!({"method":3,"send_code":true}))
                )
                .await
                .status(),
            StatusCode::ACCEPTED
        );
        fixture.phase("reauth_required").await;
    }
    assert_eq!(
        school.sent_codes.load(Ordering::SeqCst),
        1,
        "resend cooldown prevents duplicate SMS"
    );
    assert_eq!(
        fixture
            .request(
                "/api/reauth",
                Some(serde_json::json!({"method":3,"code":"wrong"}))
            )
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    fixture.phase("reauth_required").await;
    assert!(!fixture.state.session.lock().await.ready);
    fixture.finish_code().await;
    assert!(fixture.state.session.lock().await.ready);
    let contents = std::fs::read_to_string(&fixture.config.cookie_file).unwrap();
    assert!(contents.contains("AES-256-GCM"));
    assert!(!contents.contains("mock-tgt"));
    assert!(!contents.contains("test-password"));
    // A new client recovers from disk using the same key, without entering credentials.
    let resumed = WebState::new(&fixture.config, "b".repeat(64), fixture.state.db.clone()).unwrap();
    resumed.session.lock().await.api.client =
        uestc_client::UestcClient::with_encrypted_cookie_file_and_builder(
            &fixture.config.cookie_file,
            b"test-cookie-secret",
            school.builder(),
        );
    resumed.start(Action::Resume).unwrap();
    tokio::time::timeout(Duration::from_secs(15), resumed.wake.notified())
        .await
        .unwrap();
    assert_eq!(resumed.status.lock().unwrap().phase, "monitoring");
}

#[tokio::test]
async fn rejected_password_stays_live_and_does_not_leak_upstream_errors() {
    let school = mock_school::School::new();
    let fixture = Fixture::new().await;
    fixture.use_school(&school).await;
    assert_eq!(
        fixture
            .request(
                "/api/login",
                Some(
                    serde_json::json!({"login_type":"password","username":"bad","password":"wrong"})
                )
            )
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    tokio::time::timeout(Duration::from_secs(15), fixture.state.wake.notified())
        .await
        .unwrap();
    fixture.phase("awaiting_login").await;
    let response = fixture.request("/api/status", None).await;
    let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
    assert!(!String::from_utf8_lossy(&body).contains("ST-private"));
    assert!(!String::from_utf8_lossy(&body).contains("secret-upstream-error"));
    assert_eq!(
        fixture.request("/healthz", None).await.status(),
        StatusCode::OK
    );
    fixture.password_login().await;
}

#[tokio::test]
async fn qr_remains_observable_cancel_releases_session_and_scan_can_restart() {
    let school = mock_school::School::new();
    let fixture = Fixture::new().await;
    fixture.use_school(&school).await;
    assert_eq!(
        fixture
            .request(
                "/api/login",
                Some(serde_json::json!({"login_type":"wechat"}))
            )
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    fixture.phase("qr_pending").await;
    let status = fixture.state.status.lock().unwrap().clone();
    assert!(status.qr_svg.unwrap().contains("<svg"));
    assert_eq!(
        fixture.request("/api/status", None).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        fixture
            .request(
                "/api/login",
                Some(serde_json::json!({"login_type":"wechat"}))
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        fixture
            .request("/api/cancel", Some(serde_json::json!({})))
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    fixture.phase("awaiting_login").await;
    assert!(fixture.state.status.lock().unwrap().qr_svg.is_none());
    assert_eq!(
        fixture
            .request(
                "/api/login",
                Some(serde_json::json!({"login_type":"wechat"}))
            )
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    fixture.phase("qr_pending").await;
    school.confirmed.store(true, Ordering::SeqCst);
    fixture.phase("monitoring").await;
}

#[tokio::test]
async fn successful_web_login_wakes_monitor_and_expiration_pauses_without_exit() {
    let school = mock_school::School::new();
    let fixture = Fixture::new().await;
    fixture.use_school(&school).await;
    let db = DbService::new(fixture.config.database_url.clone())
        .await
        .unwrap();
    db.init().await.unwrap();
    let scenario = async {
        fixture.password_login().await;
        fixture.finish_code().await;
        tokio::time::timeout(Duration::from_secs(15), async {
            while fixture.state.status.lock().unwrap().reading.is_none() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        // Failed session checks must not turn an upstream outage into expired credentials.
        school.stage.store(3, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(15), async {
            while !fixture
                .state
                .status
                .lock()
                .unwrap()
                .message
                .starts_with("本次采集失败")
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(fixture.state.status.lock().unwrap().phase, "monitoring");
        assert!(fixture.state.session.lock().await.ready);
        school.stage.store(0, Ordering::SeqCst);
        fixture.phase("awaiting_login").await;
        assert!(!fixture.state.session.lock().await.ready);
        assert!(fixture.state.status.lock().unwrap().reading.is_some());
        assert_eq!(
            fixture.request("/healthz", None).await.status(),
            StatusCode::OK
        );
        fixture.password_login().await;
        fixture.finish_code().await;
    };
    tokio::select! {
        _ = monitor(fixture.state.clone(), db, fixture.config.clone()) => panic!("monitor exited"),
        _ = scenario => {},
    }
}

#[tokio::test]
async fn secrets_are_published_atomically_and_cookie_key_survives_credentials_changes() {
    let fixture = Fixture::new().await;
    let path = fixture.directory.join("shared-key");
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || persistent_secret(&path).unwrap())
        })
        .collect();
    let secrets: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(secrets.iter().all(|s| s == &secrets[0]));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let mut config = fixture.config.clone();
    config.cookie_encryption_key = None;
    prepare_cookie_key(&mut config).unwrap();
    let key = config.cookie_encryption_key.clone();
    config.cookie_encryption_key = None;
    config.username = Some("alice".into());
    config.password = Some("new-password".into());
    prepare_cookie_key(&mut config).unwrap();
    assert_eq!(config.cookie_encryption_key, key);
}

#[tokio::test]
async fn wechat_second_factor_uses_the_browser_qr_callback() {
    let school = mock_school::School::new();
    let fixture = Fixture::new().await;
    fixture.use_school(&school).await;
    fixture.password_login().await;
    assert_eq!(
        fixture
            .request(
                "/api/reauth",
                Some(serde_json::json!({"method":8,"trust_device":true}))
            )
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    fixture.phase("qr_pending").await;
    school.confirmed.store(true, Ordering::SeqCst);
    fixture.phase("monitoring").await;
}

#[tokio::test]
async fn unreadable_existing_cookies_are_retained_until_successful_login() {
    let fixture = Fixture::new().await;
    let original = "invalid encrypted cookie file";
    std::fs::write(&fixture.config.cookie_file, original).unwrap();
    let state = WebState::new(&fixture.config, "b".repeat(64), fixture.state.db.clone()).unwrap();
    assert_eq!(
        std::fs::read_to_string(&fixture.config.cookie_file).unwrap(),
        original
    );
    assert_eq!(state.status.lock().unwrap().phase, "awaiting_login");
}

#[tokio::test]
async fn config_view_masks_secrets() {
    let fixture = Fixture::new().await;
    let response = fixture.request("/api/config", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // 凭据与密钥只能以占位符出现，原文不得离开服务端。
    assert_eq!(value["config"]["cookie_encryption_key"], SECRET_MASK);
    assert!(!text.contains("test-cookie-secret"));
    assert_eq!(value["config"]["interval_seconds"], 1);
    assert_eq!(value["config"]["notify"]["threshold"], 5.0);
    assert!(value.get("warnings").is_none());
}

#[tokio::test]
async fn config_parse_errors_never_return_source_values() {
    let mut fixture = Fixture::new().await;
    for content in [
        "password = \"regression-fake-secret\" broken\n".to_string(),
        format!(
            "{}login_type = \"regression-fake-secret\"\n",
            fixture.config_file_content(5)
        ),
    ] {
        fixture.use_config_file(&content);
        for endpoint in ["/api/config", "/api/config/reload"] {
            let response = fixture
                .request(endpoint, Some(serde_json::json!({"interval_seconds": 7})))
                .await;
            assert!(!response.status().is_success());
            let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
            assert!(
                !String::from_utf8_lossy(&body).contains("regression-fake-secret"),
                "{endpoint} leaked configuration source values"
            );
        }
    }
}

#[tokio::test]
async fn config_save_writes_file_preserves_comments_and_applies_immediately() {
    let mut fixture = Fixture::new().await;
    let path = fixture.use_config_file(&fixture.config_file_content(5));
    let value = fixture
        .config_json(serde_json::json!({
            "interval_seconds": 7,
            "notify": {"threshold": 3.5, "heartbeat_hours": [9, 21], "enabled": true}
        }))
        .await;
    assert_eq!(value["config"]["interval_seconds"], 7);
    assert_eq!(value["config"]["notify"]["threshold"], 3.5);
    assert_eq!(
        value["config"]["notify"]["heartbeat_hours"],
        serde_json::json!([9, 21])
    );
    assert!(value.get("warnings").is_none());
    assert!(value.get("restart").is_none());
    let written = std::fs::read_to_string(&path).unwrap();
    // 保留手写注释与既有键，只增量修改被编辑的项。
    assert!(written.contains("# 测试配置"));
    assert!(written.contains("interval_seconds = 7"));
    assert!(written.contains("[notify]"));
    // 运行时配置立即跟随，无需重启。
    let applied = fixture.state.config_snapshot();
    assert_eq!(applied.interval_seconds, 7);
    assert_eq!(applied.notify.threshold, 3.5);
    assert_eq!(applied.notify.heartbeat_hours.as_slice(), &[9, 21]);
    assert!(applied.notify.enabled);
}

#[tokio::test]
async fn config_save_rejects_invalid_values_without_touching_the_file() {
    let mut fixture = Fixture::new().await;
    let path = fixture.use_config_file(&fixture.config_file_content(5));
    let original = std::fs::read_to_string(&path).unwrap();
    let response = fixture
        .request(
            "/api/config",
            Some(serde_json::json!({"interval_seconds": 0})),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(value["message"].as_str().unwrap().contains("配置校验失败"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    // 运行时快照仍是启动时加载的配置（interval=1 来自 Fixture）。
    assert_eq!(fixture.state.config_snapshot().interval_seconds, 1);
}

#[tokio::test]
async fn config_save_and_reload_reject_invalid_timezone() {
    let mut fixture = Fixture::new().await;
    let path = fixture.use_config_file(&fixture.config_file_content(5));
    let original = std::fs::read_to_string(&path).unwrap();
    let response = fixture
        .request(
            "/api/config",
            Some(serde_json::json!({"timezone": "Not/A-Real-Timezone"})),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    assert_eq!(fixture.state.config_snapshot().timezone, "Asia/Shanghai");

    std::fs::write(
        &path,
        format!("{original}timezone = \"Not/A-Real-Timezone\"\n"),
    )
    .unwrap();
    let response = fixture
        .request("/api/config/reload", Some(serde_json::json!({})))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(fixture.state.config_snapshot().timezone, "Asia/Shanghai");
}

#[tokio::test]
async fn config_save_keeps_file_unchanged_if_cookie_key_preparation_fails() {
    let mut fixture = Fixture::new().await;
    let path = fixture.use_config_file(&fixture.config_file_content(5));
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        format!("{}.key", fixture.config.cookie_file),
        "invalid-short-key",
    )
    .unwrap();
    let response = fixture
        .request(
            "/api/config",
            Some(serde_json::json!({"interval_seconds": 7})),
        )
        .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    assert_eq!(fixture.state.config_snapshot().interval_seconds, 1);
}

#[tokio::test]
async fn config_save_can_clear_a_secret() {
    let mut fixture = Fixture::new().await;
    let path = fixture.use_config_file(&format!(
        "# 测试配置\ninterval_seconds = 5\ndatabase_url = \"sqlite://{}\"\ncookie_file = \"{}\"\npassword = \"old-password\"\n",
        fixture.directory.join("monitor.db").display(),
        fixture.config.cookie_file
    ));
    let value = fixture
        .config_json(serde_json::json!({"password": null}))
        .await;
    assert_eq!(value["config"]["password"], serde_json::Value::Null);
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(!written.contains("old-password"));
}

#[tokio::test]
async fn config_save_replaces_the_heartbeat_hour_alias_without_duplicates() {
    let mut fixture = Fixture::new().await;
    let path = fixture.use_config_file(&format!(
        "# 测试配置\ninterval_seconds = 5\ndatabase_url = \"sqlite://{}\"\ncookie_file = \"{}\"\n[notify]\nheartbeat_hour = [8]\n",
        fixture.directory.join("monitor.db").display(),
        fixture.config.cookie_file
    ));
    let value = fixture
        .config_json(serde_json::json!({"notify": {"heartbeat_hours": [9, 21]}}))
        .await;
    assert_eq!(
        value["config"]["notify"]["heartbeat_hours"],
        serde_json::json!([9, 21])
    );
    let written = std::fs::read_to_string(&path).unwrap();
    // 别名 heartbeat_hour 必须被移除：与 heartbeat_hours 并存会让 serde 报重复字段。
    assert!(!written.contains("heartbeat_hour = [8]"));
    assert!(written.contains("heartbeat_hours = [9, 21]"));
}

#[tokio::test]
async fn config_reload_picks_up_manual_file_edits() {
    let mut fixture = Fixture::new().await;
    fixture.use_config_file(&fixture.config_file_content(5));
    let path = fixture
        .state
        .config_file
        .clone()
        .expect("config file seeded");
    std::fs::write(&path, fixture.config_file_content(9)).unwrap();
    let response = fixture
        .request("/api/config/reload", Some(serde_json::json!({})))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fixture.state.config_snapshot().interval_seconds, 9);
}

// ---------------- 用电趋势 ----------------

fn trend_record(created_at: &str, money: f64, energy: f64) -> crate::db::PowerRecord {
    crate::db::PowerRecord {
        remaining_money: money,
        remaining_energy: energy,
        room_display_name: "宿舍".into(),
        created_at: created_at.into(),
    }
}

fn shanghai_tz() -> chrono_tz::Tz {
    "Asia/Shanghai".parse().unwrap()
}

#[test]
fn trend_estimates_daily_sampling_and_excludes_partial_boundary_days() {
    let records = vec![
        trend_record("2026-09-20T08:00:00+08:00", 20.0, 20.0),
        trend_record("2026-09-21T08:00:00+08:00", 18.0, 18.0),
        trend_record("2026-09-22T08:00:00+08:00", 16.0, 16.0),
    ];
    let trend = build_trend(&records, shanghai_tz(), trend_day("2026-09-23"));
    let estimate = trend.estimate.expect("完整覆盖的 21 日应可估算");
    assert!((estimate.daily_money - 2.0).abs() < 1e-9);
    assert!((estimate.daily_energy - 2.0).abs() < 1e-9);
    assert!((estimate.days_remaining - 8.0).abs() < 1e-9);
}

#[test]
fn trend_excludes_recharge_days_even_when_daily_balance_drops() {
    let records = vec![
        trend_record("2026-09-20T00:00:00+08:00", 20.0, 20.0),
        trend_record("2026-09-20T08:00:00+08:00", 15.0, 15.0),
        trend_record("2026-09-20T12:00:00+08:00", 18.0, 18.0),
        trend_record("2026-09-20T23:50:00+08:00", 10.0, 10.0),
        trend_record("2026-09-21T00:00:00+08:00", 10.0, 10.0),
    ];
    let trend = build_trend(&records, shanghai_tz(), trend_day("2026-09-21"));
    assert!(
        trend.estimate.is_none(),
        "唯一完整天发生了充值，不能作为单价或日均样本"
    );
}

#[test]
fn trend_counts_a_complete_day_across_a_dst_change() {
    let records = vec![
        trend_record("2026-03-08T00:00:00-05:00", 20.0, 20.0),
        trend_record("2026-03-09T00:00:00-04:00", 17.7, 17.7),
    ];
    let trend = build_trend(
        &records,
        "America/New_York".parse().unwrap(),
        trend_day("2026-03-10"),
    );
    let estimate = trend.estimate.expect("23 小时的完整日应可估算");
    assert!((estimate.daily_money - 2.3).abs() < 1e-9);
    assert!((estimate.daily_energy - 2.3).abs() < 1e-9);
}

#[test]
fn trend_requires_usage_in_the_last_seven_calendar_days() {
    let records = vec![
        trend_record("2026-09-10T00:00:00+08:00", 20.0, 20.0),
        trend_record("2026-09-11T00:00:00+08:00", 18.0, 18.0),
    ];
    let trend = build_trend(&records, shanghai_tz(), trend_day("2026-09-30"));
    assert!(
        trend.estimate.is_none(),
        "不应使用近七天之外的旧用量代替近期日均"
    );
}

fn trend_day(date: &str) -> chrono::NaiveDate {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap()
}

#[test]
fn build_trend_aggregates_daily_usage_and_estimates_days_remaining() {
    let records = vec![
        trend_record("2026-09-20T00:00:00+08:00", 20.0, 10.0),
        trend_record("2026-09-20T12:00:00+08:00", 19.0, 9.0),
        trend_record("2026-09-21T00:00:00+08:00", 18.0, 8.0),
        trend_record("2026-09-21T12:00:00+08:00", 16.5, 6.5),
        trend_record("2026-09-22T00:00:00+08:00", 15.0, 5.0),
        trend_record("2026-09-22T06:00:00+08:00", 14.5, 4.5),
    ];
    let trend = build_trend(&records, shanghai_tz(), trend_day("2026-09-22"));
    assert_eq!(trend.days.len(), 3);
    assert_eq!(trend.days[0].date, trend_day("2026-09-20"));
    // 每日快照取真实的末次读数；全天用量包含跨午夜区间。
    assert_eq!(trend.days[0].money, 19.0);
    assert_eq!(trend.days[0].used_money, 2.0);
    assert_eq!(trend.days[0].used_energy, 2.0);
    assert_eq!(trend.days[1].used_money, 3.0);
    assert_eq!(trend.days[2].money, 14.5);
    let estimate = trend.estimate.expect("两个完整天应给出估算");
    assert_eq!(estimate.daily_money, 2.5);
    assert_eq!(estimate.daily_energy, 2.5);
    assert_eq!(estimate.days_remaining, 5.8);
}

#[test]
fn build_trend_falls_back_to_unit_price_when_recent_days_all_recharged() {
    let mut records = Vec::new();
    let mut money = 20.0;
    let mut energy = 20.0;
    // 三个正常天：每天 2 元 / 2 kWh（单价 1 元/kWh）
    for day in 14..=16 {
        records.push(trend_record(
            &format!("2026-09-{day:02}T08:00:00+08:00"),
            money,
            energy,
        ));
        money -= 2.0;
        energy -= 2.0;
        records.push(trend_record(
            &format!("2026-09-{day:02}T22:00:00+08:00"),
            money,
            energy,
        ));
    }
    // 七个充值日：电量继续下降，余额因充值上升（used_money 为负）
    for day in 17..=23 {
        records.push(trend_record(
            &format!("2026-09-{day:02}T08:00:00+08:00"),
            money,
            energy,
        ));
        money += 10.0;
        energy -= 2.0;
        records.push(trend_record(
            &format!("2026-09-{day:02}T22:00:00+08:00"),
            money,
            energy,
        ));
    }
    let trend = build_trend(&records, shanghai_tz(), trend_day("2026-09-24"));
    // 近 7 天全是充值日：回退到全窗口非充值天拟合的单价（1 元/kWh）折算
    let estimate = trend.estimate.expect("单价回退应给出估算");
    assert_eq!(estimate.daily_energy, 2.0);
    assert_eq!(estimate.daily_money, 2.0);
    assert_eq!(estimate.days_remaining, 42.0);
    let recharge_day = trend
        .days
        .iter()
        .find(|day| day.date == trend_day("2026-09-23"))
        .unwrap();
    assert_eq!(recharge_day.used_money, -10.0);
}

#[test]
fn build_trend_returns_no_estimate_without_complete_days() {
    let records = vec![
        trend_record("2026-09-22T08:00:00+08:00", 20.0, 10.0),
        trend_record("2026-09-22T09:00:00+08:00", 19.0, 9.0),
    ];
    let trend = build_trend(&records, shanghai_tz(), trend_day("2026-09-22"));
    assert_eq!(trend.days.len(), 1);
    assert!(trend.estimate.is_none());
}

#[tokio::test]
async fn trend_endpoint_aggregates_today_and_history_supports_limit() {
    let fixture = Fixture::new().await;
    for money in [30.0, 29.0, 27.5] {
        fixture
            .state
            .db
            .save_data(&crate::api::PowerInfo {
                code: 0,
                message: "ok".into(),
                remaining_money: money,
                remaining_energy: money * 2.0,
                room_display_name: "宿舍".into(),
                meter_room_id: "m".into(),
                room_id: "r".into(),
                building_id: "b".into(),
                campus_id: "c".into(),
                room_number: "1".into(),
            })
            .await
            .unwrap();
    }
    let response = fixture.request("/api/trend", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let days = value["days"].as_array().unwrap();
    assert_eq!(days.len(), 1);
    assert_eq!(days[0]["money"], 27.5);
    assert_eq!(days[0]["used_money"], 2.5);
    assert_eq!(days[0]["used_energy"], 5.0);
    // 只有当天（不完整）的数据时无从估算
    assert!(value["estimate"].is_null());
    let response = fixture.request("/api/history?limit=2", None).await;
    let body: Vec<serde_json::Value> =
        serde_json::from_slice(&to_bytes(response.into_body(), 16 * 1024).await.unwrap()).unwrap();
    assert_eq!(body.len(), 2);
    let response = fixture.request("/api/history", None).await;
    let body: Vec<serde_json::Value> =
        serde_json::from_slice(&to_bytes(response.into_body(), 16 * 1024).await.unwrap()).unwrap();
    assert_eq!(body.len(), 3);
}

#[tokio::test]
async fn trend_endpoint_preserves_latest_reading_beyond_20000_samples() {
    let fixture = Fixture::new().await;
    let pool = sqlx::SqlitePool::connect(&fixture.config.database_url)
        .await
        .unwrap();
    sqlx::query(
        "WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n < 20001) \
         INSERT INTO power_records (remaining_energy, remaining_money, meter_room_id, \
             room_display_name, room_id, building_id, campus_id, room_number, created_at) \
         SELECT (20002-n)*2, 20002-n, 'm', 'room', 'r', 'b', 'c', '1', ? FROM seq",
    )
    .bind(crate::time::now_rfc3339())
    .execute(&pool)
    .await
    .unwrap();
    let response = fixture.request("/api/trend", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["days"][0]["money"], 1.0);
    assert_eq!(value["days"][0]["energy"], 2.0);
    pool.close().await;
}

// ---------------- 测试通知 ----------------

#[tokio::test]
async fn notify_test_reports_per_channel_results() {
    let fixture = Fixture::new().await;
    let response = fixture
        .request("/api/notify/test", Some(serde_json::json!({})))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(value["message"].as_str().unwrap().contains("通知未启用"));
    assert_eq!(value["results"].as_array().unwrap().len(), 0);

    let mut config = fixture.config.clone();
    config.notify.enabled = true;
    config.notify.notify_types = vec![crate::config::NotifyType::Console];
    fixture.state.swap_config(config);
    let response = fixture
        .request("/api/notify/test", Some(serde_json::json!({})))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(value.get("message").is_none());
    let results = value["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["channel"], "console");
    assert_eq!(results[0]["ok"], true);
}
