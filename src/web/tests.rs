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
