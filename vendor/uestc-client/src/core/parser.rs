use crate::{Result, UestcClientError};
use scraper::{Html, Selector};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct LoginPageInfo {
    /// The URL path to the encryption script
    #[allow(unused)]
    pub encrypt_script_path: Option<String>,
    /// The value of the input with id "pwdEncryptSalt"
    pub pwd_encrypt_salt: String,
    /// All other input fields found in the login form (id -> value)
    pub form_data: HashMap<String, String>,
}

pub fn parse_login_page(html: &str) -> Result<LoginPageInfo> {
    log::debug!("Parsing login page HTML ({} bytes)", html.len());

    let document = Html::parse_document(html);

    // Find the encryption script path
    let script_selector = Selector::parse("script[type='text/javascript']").map_err(|e| {
        UestcClientError::HtmlParseError {
            message: format!("Failed to parse script selector: {:?}", e),
            source: None,
        }
    })?;

    let mut encrypt_script_path = None;
    for element in document.select(&script_selector) {
        if let Some(src) = element.value().attr("src") {
            if src.contains("encrypt") {
                encrypt_script_path = Some(src.to_string());
                break;
            }
        }
    }

    // Parse form data
    let form_selector =
        Selector::parse("div#pwdLoginDiv input").map_err(|e| UestcClientError::HtmlParseError {
            message: format!("Failed to parse form selector: {:?}", e),
            source: None,
        })?;

    let mut form_data = HashMap::new();
    let mut pwd_encrypt_salt = None;

    for element in document.select(&form_selector) {
        let input_id = element.value().attr("id").unwrap_or("N/A");
        let input_value = element.value().attr("value").unwrap_or("N/A");

        if input_id == "pwdEncryptSalt" {
            pwd_encrypt_salt = Some(input_value.to_string());
        }

        if input_id != "N/A" && input_value != "N/A" {
            form_data.insert(input_id.to_string(), input_value.to_string());
        }
    }

    log::debug!("Found {} form fields in login page", form_data.len());

    // Python: assert pwdEncryptSalt, "Failed to get pwdEncryptSalt"
    let pwd_encrypt_salt = pwd_encrypt_salt.ok_or_else(|| {
        log::error!("Failed to find 'pwdEncryptSalt' field in login page");
        UestcClientError::HtmlParseError {
            message: "Failed to find 'pwdEncryptSalt' in login page".to_string(),
            source: None,
        }
    })?;

    log::debug!(
        "Successfully parsed login page (salt length: {} bytes)",
        pwd_encrypt_salt.len()
    );

    Ok(LoginPageInfo {
        encrypt_script_path,
        pwd_encrypt_salt,
        form_data,
    })
}

/// Extract error message from login response HTML
pub fn extract_error_message(html: &str) -> Option<String> {
    let document = Html::parse_document(html);
    let error_selector = Selector::parse("span#showErrorTip").ok()?;

    let error_msg = document.select(&error_selector).next().and_then(|el| {
        let text = el.text().collect::<String>().trim().to_string();
        if text.is_empty() { None } else { Some(text) }
    });

    if let Some(ref msg) = error_msg {
        log::debug!("Extracted error message from login page: {}", msg);
    }

    error_msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_parse_login_page() {
        // 请求真实的登录页面
        let html = reqwest::get("https://idas.uestc.edu.cn/authserver/login?service=https%3A%2F%2Feportal.uestc.edu.cn%2Flogin%3Fservice%3Dhttps%3A%2F%2Feportal.uestc.edu.cn%2Fnew%2Findex.html%3Fbrowser%3Dno")
            .await
            .expect("Failed to request login page")
            .text()
            .await
            .expect("Failed to get response text");

        // 解析页面
        let result = parse_login_page(&html);
        assert!(result.is_ok(), "Parsing failed: {:?}", result.err());

        let info = result.unwrap();

        // 验证解析结果
        // 验证加密脚本路径
        assert!(
            info.encrypt_script_path.is_some(),
            "encrypt_script_path should be found"
        );
        let script_path = info.encrypt_script_path.as_ref().unwrap();
        assert!(
            script_path.contains("encrypt"),
            "script path should contain 'encrypt'"
        );

        // 验证 pwdEncryptSalt
        assert!(
            !info.pwd_encrypt_salt.is_empty(),
            "pwd_encrypt_salt should not be empty"
        );

        // 验证表单数据
        assert!(!info.form_data.is_empty(), "form_data should not be empty");
    }
}
