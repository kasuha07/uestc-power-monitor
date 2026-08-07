//! reauth（多因子二次认证）协议实现。
//!
//! 协议依据 `uestc-idas-analysis`（2026-08-06 wire + 源码双验证）：
//! - 密码登录成功后服务端 302 到 `reAuthCheck/reAuthLoginView.do?isMultifactor=true`，
//!   TGT 已发放但处于"待 reauth"锁定态——未完成第二因素前无法换取 ST；
//! - reauth 页一切参数来自服务端渲染的 `var reAuthParams={...}`（键带双引号、值可能含
//!   HTML 片段），且页面可能有多个同名块，必须取「目标键命中最多」的块；
//! - 可用方式 = 服务端渲染的 Tab 结构（`reauth-tab-item` 平铺 + `reauth-tab-more-item`
//!   下拉），未渲染 = 未开通 = 切换被拒（code:0）+ 提交必失败；
//! - 提交接口 `POST /reAuthCheck/reAuthSubmit.do`（10 键全量，未用键=空串）；
//! - 切换 `POST /reAuthCheck/changeReAuthType.do`（code:1 成功 / code:0 服务端拒绝）；
//! - 发码 `POST /dynamicCode/getDynamicCodeByReauth.do`（res=success 等）；
//! - 微信扫码（type 8/16）不走 reAuthSubmit.do，而是外部 OAuth 链路
//!   `combinedLogin.do?type=weixin&reAuth=2`（扫码即完成第二因素）。

use crate::{Result, UestcClientError};
use regex::Regex;

/// reauth 页路径特征（用于识别"登录后落在 reauth 页"）。
pub const REAUTH_URL_MARKERS: [&str; 2] = ["reAuthCheck", "reAuthLoginView"];

/// 提交 reauth 类接口时的 Referer（与浏览器一致）。
pub const REAUTH_REFERER: &str =
    "https://idas.uestc.edu.cn/authserver/reAuthCheck/reAuthLoginView.do?isMultifactor=true";

/// 动态码方式 → 发码接口的 `authCodeTypeName`（getDynamicCodeByReauth.do 字段）。
fn auth_code_type_name(id: i32) -> Option<&'static str> {
    Some(match id {
        3 => "reAuthDynamicCodeType",
        4 => "reAuthWChatDynamicCodeType",
        5 => "reAuthCpdailyDynamicCodeType",
        11 => "reAuthEmailDynamicCodeType",
        12 => "reAuthDingTalkDynamicCodeType",
        13 => "reAuthWeLinkDynamicCodeType",
        15 => "reAuthWeChatServiceDynamicCodeType",
        _ => return None,
    })
}

/// reauth 方式的大类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReauthMethodKind {
    /// 密码（type 2）：提交 `password` 字段（reauth 页自有 salt 加密）
    Password,
    /// 动态码（3/4/5/11/12/13/15）：发码后提交 `dynamicCode`（明文）
    DynamicCode,
    /// 微信扫码（8/16）：外部 OAuth，不走 reAuthSubmit.do
    Wechat,
    /// 其它未支持方式（如人脸 6/18、OTP 10、QQ 9、设备扫码 14）
    Unsupported,
}

/// 一个可用的 reauth 方式（来自服务端渲染的 Tab）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReauthMethod {
    /// 方式编号（Tab 的 data-type / id）
    pub id: i32,
    /// 显示名（Tab 的 data-name，服务端渲染）
    pub name: String,
    /// 是否为当前默认方式（reauth-tab-active）
    pub current: bool,
}

impl ReauthMethod {
    pub fn kind(&self) -> ReauthMethodKind {
        match self.id {
            2 => ReauthMethodKind::Password,
            3 | 4 | 5 | 11 | 12 | 13 | 15 => ReauthMethodKind::DynamicCode,
            8 | 16 => ReauthMethodKind::Wechat,
            _ => ReauthMethodKind::Unsupported,
        }
    }

    /// 库是否实现了该方式的提交流程。
    pub fn is_supported(&self) -> bool {
        self.kind() != ReauthMethodKind::Unsupported
    }
}

/// reauth 会话上下文：`login()` 返回 `ReauthRequired` 时携带，供
/// `change_reauth_type` / `send_reauth_code` / `submit_reauth` 使用。
#[derive(Debug, Clone)]
pub struct ReauthContext {
    /// 服务端渲染的可用方式（未渲染 = 未开通 = 切换被拒 + 提交必失败）
    pub available_methods: Vec<ReauthMethod>,
    /// 业务系统回调地址（服务端渲染；直登时为空串）
    pub(crate) service: String,
    /// 当前 reauth 方式编号（字符串，服务端渲染；change 成功后更新）
    pub(crate) re_auth_type: String,
    /// "true"（服务端渲染，原样回传）
    pub(crate) is_multifactor: String,
    /// 账号（发码接口需要）
    pub(crate) re_auth_user_id: String,
    /// 密码加密盐（reauth 页自有，与登录页不同；密码 reauth 才需要）
    pub(crate) pwd_encrypt_salt: Option<String>,
}

impl ReauthContext {
    /// 当前 reauth 方式编号（服务端渲染；`change_reauth_type` 成功后更新）。
    ///
    /// 提交非当前方式会被服务端拒绝（reAuth_failed），调用方应据此决定
    /// 是否需要先切换。
    pub fn current_type_id(&self) -> i32 {
        self.re_auth_type.parse().unwrap_or(-1)
    }
}

// ---------------------------------------------------------------------------
// 页面解析
// ---------------------------------------------------------------------------

/// 从 `var reAuthParams={...}` 块中提取指定键的值。
///
/// 键支持 双引号/单引号/裸 三种写法；值支持 `"str"` / `'str'` / 裸值；
/// `null` / `undefined` 归一为 `None`。值内 HTML 片段（如 `style=color:#FF7D00>`）
/// 不会被误认成键——键匹配要求 `:` 前缀不是普通值字符。
fn extract_param(block: &str, key: &str) -> Option<String> {
    // 键必须以 `{` 或 `,` 开头（对象键的合法前置符；Rust regex 不支持
    // look-behind，用消费式锚定替代）。键本身支持 双引号/单引号/裸 三种写法。
    let pattern = format!(
        r#"(?:[{{,])\s*["']?{}\s*["']?\s*:\s*(?:"((?:[^"\\]|\\.)*)"|'((?:[^'\\]|\\.)*)'|([^,}}\s]+))"#,
        regex::escape(key)
    );
    let re = Regex::new(&pattern).ok()?;
    let caps = re.captures(block)?;
    let value = if let Some(v) = caps.get(1) {
        v.as_str()
    } else if let Some(v) = caps.get(2) {
        v.as_str()
    } else {
        caps.get(3)?.as_str()
    };
    if value == "null" || value == "undefined" {
        None
    } else {
        Some(value.to_string())
    }
}

/// 解析 reauth 页：提取 reAuthParams（多块取目标键命中最多者）+ 可用方式 Tab。
///
/// 页面可能含多个 `var reAuthParams={...}` 块（数盾 OTP 提示块在前、真块在后），
/// 真块键带双引号且值内 HTML 含未转义引号——因此不能只匹配第一个块，
/// 也不能依赖严格 JSON 解析。
pub fn parse_reauth_page(html: &str) -> Result<ReauthContext> {
    const TARGET_KEYS: [&str; 5] = [
        "service",
        "reAuthType",
        "isMultifactor",
        "reAuthUserId",
        "pwdEncryptSalt",
    ];

    let block_re = Regex::new(r"(?s)var\s+reAuthParams\s*=\s*(\{.*?\});").map_err(|e| {
        UestcClientError::HtmlParseError {
            message: format!("reAuthParams 块正则编译失败: {e}"),
            source: None,
        }
    })?;

    let mut best: Option<(&str, usize)> = None;
    for caps in block_re.captures_iter(html) {
        let block = caps.get(1).expect("capture group 1").as_str();
        let hits = TARGET_KEYS
            .iter()
            .filter(|k| extract_param(block, k).is_some())
            .count();
        if best.as_ref().is_none_or(|(_, n)| hits > *n) {
            best = Some((block, hits));
        }
    }

    let (block, hits) = best.ok_or_else(|| UestcClientError::HtmlParseError {
        message: "reauth 页未找到 reAuthParams 块（页面结构可能已变）".to_string(),
        source: None,
    })?;
    if hits == 0 {
        return Err(UestcClientError::HtmlParseError {
            message: "reAuthParams 块未包含任何目标键（页面结构可能已变）".to_string(),
            source: None,
        });
    }

    let re_auth_type =
        extract_param(block, "reAuthType").ok_or_else(|| UestcClientError::HtmlParseError {
            message: "reAuthParams 缺少 reAuthType（页面结构可能已变）".to_string(),
            source: None,
        })?;
    let is_multifactor =
        extract_param(block, "isMultifactor").ok_or_else(|| UestcClientError::HtmlParseError {
            message: "reAuthParams 缺少 isMultifactor（页面结构可能已变）".to_string(),
            source: None,
        })?;
    let re_auth_user_id =
        extract_param(block, "reAuthUserId").ok_or_else(|| UestcClientError::HtmlParseError {
            message: "reAuthParams 缺少 reAuthUserId（页面结构可能已变）".to_string(),
            source: None,
        })?;

    let available_methods = parse_reauth_tabs(html);
    if available_methods.is_empty() {
        log::warn!("reauth 页未解析到可用方式 Tab（页面结构可能已变）");
    }

    Ok(ReauthContext {
        available_methods,
        service: extract_param(block, "service").unwrap_or_default(),
        re_auth_type,
        is_multifactor,
        re_auth_user_id,
        pwd_encrypt_salt: extract_param(block, "pwdEncryptSalt"),
    })
}

/// 提取 reauth 页可用方式列表（服务端渲染的 Tab 结构，2026-08-06 实测）：
/// - `reauth-tab-item`：平铺 Tab（data-type / data-name）
/// - `reauth-tab-more-item`：「更多」下拉项（id / data-name）
/// - `reauth-tab-active`：当前默认方式
///
/// 服务端只渲染本账号开通的方式；未渲染 = 未开通 = 切换被拒(code:0) + 提交必失败。
/// 平铺与下拉去重合并，按文档序返回。
pub fn parse_reauth_tabs(html: &str) -> Vec<ReauthMethod> {
    let mut out: Vec<ReauthMethod> = Vec::new();

    let active_re = Regex::new(r#"reauth-tab-item[^>]*reauth-tab-active[^>]*data-type="(\d+)""#)
        .expect("valid active tab regex");
    let active: std::collections::HashSet<String> = active_re
        .captures_iter(html)
        .filter_map(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .collect();

    let item_re = Regex::new(r#"reauth-tab-item[^>]*data-type="(\d+)"[^>]*data-name="([^"]+)""#)
        .expect("valid tab item regex");
    for caps in item_re.captures_iter(html) {
        let id = caps[1].to_string();
        if out.iter().any(|m| m.id.to_string() == id) {
            continue;
        }
        out.push(ReauthMethod {
            id: caps[1].parse().unwrap_or(-1),
            name: caps[2].to_string(),
            current: active.contains(&id),
        });
    }

    let more_re = Regex::new(r#"reauth-tab-more-item[^>]*id="(\d+)"[^>]*data-name="([^"]+)""#)
        .expect("valid tab more regex");
    for caps in more_re.captures_iter(html) {
        let id = caps[1].to_string();
        if out.iter().any(|m| m.id.to_string() == id) {
            continue;
        }
        out.push(ReauthMethod {
            id: caps[1].parse().unwrap_or(-1),
            name: caps[2].to_string(),
            current: active.contains(&id),
        });
    }

    out
}

// ---------------------------------------------------------------------------
// 表单构造（wire 实测字段，async/blocking 共用）
// ---------------------------------------------------------------------------

/// changeReAuthType.do 请求体：isMultifactor / reAuthType / service（40B wire 实测）。
pub fn change_form(ctx: &ReauthContext, method_id: i32) -> Vec<(&'static str, String)> {
    vec![
        ("isMultifactor", ctx.is_multifactor.clone()),
        ("reAuthType", method_id.to_string()),
        ("service", ctx.service.clone()),
    ]
}

/// getDynamicCodeByReauth.do 请求体：userName / authCodeTypeName（60B wire 实测）。
pub fn send_code_form(ctx: &ReauthContext, method_id: i32) -> Result<Vec<(&'static str, String)>> {
    let type_name =
        auth_code_type_name(method_id).ok_or_else(|| UestcClientError::ReauthFailed {
            message: format!("方式 {method_id} 不是动态码方式，无法发码"),
        })?;
    Ok(vec![
        ("userName", ctx.re_auth_user_id.clone()),
        ("authCodeTypeName", type_name.to_string()),
    ])
}

/// reAuthSubmit.do 请求体（10 键全量，未用键=空串，wire 实测 122B）。
pub fn submit_form(
    ctx: &ReauthContext,
    code: Option<&str>,
    encrypted_password: Option<String>,
    skip_tmp_reauth: bool,
) -> Vec<(&'static str, String)> {
    let mut form = vec![
        ("service", ctx.service.clone()),
        ("reAuthType", ctx.re_auth_type.clone()),
        ("isMultifactor", ctx.is_multifactor.clone()),
        ("password", encrypted_password.unwrap_or_default()),
        ("dynamicCode", code.unwrap_or("").to_string()),
        ("uuid", String::new()),
        ("answer1", String::new()),
        ("answer2", String::new()),
        ("otpCode", String::new()),
    ];
    // 可信设备弹窗注入的第 10 键：true=信任此设备（服务端持久化指纹）/ false=仅本次
    form.push(("skipTmpReAuth", skip_tmp_reauth.to_string()));
    form
}

/// 近空表单：reauth 完成后 /login 停在渲染页时手动 POST 换取 ST（wire 实测）。
pub fn near_empty_login_form(execution: &str) -> Vec<(&'static str, String)> {
    vec![
        ("username", String::new()),
        ("password", String::new()),
        ("captcha", String::new()),
        ("_eventId", "submit".to_string()),
        ("cllt", "userNameLogin".to_string()),
        ("dllt", "generalLogin".to_string()),
        ("lt", String::new()),
        ("execution", execution.to_string()),
    ]
}

// ---------------------------------------------------------------------------
// 响应判读
// ---------------------------------------------------------------------------

/// changeReAuthType.do 响应：code==1 成功；code==0 = 服务端拒绝（未开通）。
pub fn parse_change_response(body: &str) -> Result<()> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| UestcClientError::ReauthFailed {
            message: format!("changeReAuthType 响应不是 JSON: {}", truncate(body)),
        })?;
    let code = value.get("code");
    if matches!(code, Some(serde_json::Value::Number(n)) if n.as_i64() == Some(1))
        || matches!(code, Some(serde_json::Value::String(s)) if s == "1")
    {
        return Ok(());
    }
    let msg = value
        .get("msg")
        .and_then(|v| v.as_str())
        .unwrap_or("服务端拒绝切换方式");
    Err(UestcClientError::ReauthFailed {
        message: format!("切换 reauth 方式被拒（code:0）: {msg}"),
    })
}

/// getDynamicCodeByReauth.do 响应：res 在成功集合内即发码成功。
pub fn parse_send_code_response(body: &str) -> Result<()> {
    const OK_RES: [&str; 4] = [
        "success",
        "wechat_success",
        "cpdaily_success",
        "other_success",
    ];
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| UestcClientError::ReauthFailed {
            message: format!("getDynamicCodeByReauth 响应不是 JSON: {}", truncate(body)),
        })?;
    let res = value
        .get("res")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    if OK_RES.contains(&res) {
        return Ok(());
    }
    let msg = value
        .get("returnMessage")
        .and_then(|v| v.as_str())
        .unwrap_or(res);
    Err(UestcClientError::ReauthFailed {
        message: format!("发送验证码失败: {msg}"),
    })
}

/// reAuthSubmit.do 响应：reAuth_failed / reAuth_unauthorized = 失败；其余视为成功。
pub fn parse_submit_response(body: &str) -> Result<()> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| UestcClientError::ReauthFailed {
            message: format!("reAuthSubmit 响应不是 JSON: {}", truncate(body)),
        })?;
    let code = value.get("code").and_then(|v| v.as_str()).unwrap_or("");
    if code == "reAuth_failed" || code == "reAuth_unauthorized" {
        let msg = value
            .get("msg")
            .and_then(|v| v.as_str())
            .unwrap_or("认证失败");
        return Err(UestcClientError::ReauthFailed {
            message: format!("reauth 提交失败: {msg}"),
        });
    }
    Ok(())
}

/// 日志用的截断响应体（响应只进错误消息，限长防刷屏）。
fn truncate(body: &str) -> String {
    let mut out: String = body.chars().take(200).collect();
    if body.chars().count() > 200 {
        out.push_str("...<truncated>");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实 reauth 页样本（2026-08-06 抓取，`work/reauth-page-1786032273641.html`，
    /// 学号已替换为占位符）。含数盾 OTP 假块在前、真块在后的结构，
    /// 用于验证「取目标键命中最多块」与「键带双引号」解析。
    const REAUTH_PAGE: &str = include_str!("../../tests/fixtures/reauth-page.html");

    #[test]
    fn parses_real_reauth_page() {
        let ctx = parse_reauth_page(REAUTH_PAGE).expect("should parse");
        assert_eq!(ctx.re_auth_type, "8");
        assert_eq!(ctx.is_multifactor, "true");
        assert_eq!(ctx.re_auth_user_id, "2025XXXXXXXX");
        assert_eq!(ctx.service, "");
        assert_eq!(ctx.pwd_encrypt_salt.as_deref(), Some("CirR5KGpTcdlxnqJ"));
    }

    #[test]
    fn parses_reauth_tabs_from_real_page() {
        let tabs = parse_reauth_tabs(REAUTH_PAGE);
        let ids: Vec<i32> = tabs.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![8, 3, 4]);
        assert_eq!(tabs[0].name, "微信扫码");
        assert!(tabs[0].current, "tab 8 应为当前默认方式");
        assert!(!tabs[1].current);
        assert_eq!(tabs[1].name, "短信验证码");
        assert_eq!(tabs[2].name, "企业微信验证码");
    }

    #[test]
    fn picks_the_block_with_most_target_keys() {
        // 数盾 OTP 假块（键无目标键）排在前，真块在后 → 必须选真块
        let html = r##"<script>
            var reAuthParams = {color:"#FF7D00>",_jar:{map:{},file:null}};
            var reAuthParams = {"isSleepAccount":"0","service":null,"reAuthType":"3",
                "isMultifactor":"true","reAuthUserId":"2025XXXXXXXX","pwdEncryptSalt":"AbcDef1234567890"};
        </script>"##;
        let ctx = parse_reauth_page(html).expect("should parse");
        assert_eq!(ctx.re_auth_type, "3");
        assert_eq!(ctx.pwd_encrypt_salt.as_deref(), Some("AbcDef1234567890"));
    }

    #[test]
    fn supports_bare_and_single_quoted_keys() {
        let html = r#"<script>
            var reAuthParams = {reAuthType: '4', isMultifactor: "true", reAuthUserId: 123, service: null};
        </script>"#;
        let ctx = parse_reauth_page(html).expect("should parse");
        assert_eq!(ctx.re_auth_type, "4");
        assert_eq!(ctx.re_auth_user_id, "123");
        assert_eq!(ctx.service, "");
    }

    #[test]
    fn rejects_page_without_reauth_params() {
        let err = parse_reauth_page("<html>nothing here</html>").expect_err("should fail");
        assert!(err.to_string().contains("reAuthParams"));
    }

    #[test]
    fn tabs_are_deduplicated_between_tile_and_more() {
        let html = r#"<div class="reauth-tab-item" data-type="8" data-name="微信扫码"></div>
            <div class="reauth-tab-more-item" id="8" data-name="微信扫码"></div>"#;
        let tabs = parse_reauth_tabs(html);
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].id, 8);
    }

    #[test]
    fn method_kinds_are_classified() {
        let method = |id: i32| ReauthMethod {
            id,
            name: String::new(),
            current: false,
        };
        assert_eq!(method(2).kind(), ReauthMethodKind::Password);
        assert_eq!(method(3).kind(), ReauthMethodKind::DynamicCode);
        assert_eq!(method(15).kind(), ReauthMethodKind::DynamicCode);
        assert_eq!(method(8).kind(), ReauthMethodKind::Wechat);
        assert_eq!(method(16).kind(), ReauthMethodKind::Wechat);
        assert_eq!(method(6).kind(), ReauthMethodKind::Unsupported);
        assert!(!method(6).is_supported());
        assert!(method(3).is_supported());
    }

    #[test]
    fn submit_form_has_all_ten_keys() {
        let ctx = ReauthContext {
            available_methods: vec![],
            service: String::new(),
            re_auth_type: "3".to_string(),
            is_multifactor: "true".to_string(),
            re_auth_user_id: "2025XXXXXXXX".to_string(),
            pwd_encrypt_salt: None,
        };
        let form = submit_form(&ctx, Some("123456"), None, false);
        let map: std::collections::HashMap<_, _> = form.into_iter().collect();
        assert_eq!(map["reAuthType"], "3");
        assert_eq!(map["dynamicCode"], "123456");
        assert_eq!(map["password"], "");
        assert_eq!(map["answer1"], "");
        assert_eq!(map["answer2"], "");
        assert_eq!(map["otpCode"], "");
        assert_eq!(map["skipTmpReAuth"], "false");
        assert_eq!(map.len(), 10);
    }

    #[test]
    fn response_parsing() {
        assert!(parse_change_response(r#"{"code":1,"data":{"reAuthType":3}}"#).is_ok());
        assert!(parse_change_response(r#"{"code":0,"data":null}"#).is_err());
        assert!(parse_change_response(r#"{"code":"1"}"#).is_ok());

        assert!(parse_send_code_response(r#"{"res":"success"}"#).is_ok());
        assert!(parse_send_code_response(r#"{"res":"wechat_success"}"#).is_ok());
        assert!(parse_send_code_response(r#"{"res":"code_time_fail"}"#).is_err());

        assert!(parse_submit_response(r#"{"code":"ok"}"#).is_ok());
        assert!(parse_submit_response(r#"{"msg":"验证码错误","code":"reAuth_failed"}"#).is_err());
        assert!(parse_submit_response(r#"{"code":"reAuth_unauthorized"}"#).is_err());
    }
}
