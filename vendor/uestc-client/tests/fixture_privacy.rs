use regex::Regex;
use std::{fs, path::Path};

/// A closed vocabulary, not a PII denylist: unreviewed text, attributes and JS
/// fields fail even when they contain an unfamiliar name or an encoded value.
/// Keep the synthetic values explicit; never add captured values to this list.
fn reauth_fixture_is_synthetic(html: &str) -> bool {
    let allowed = Regex::new(
        r##"(?x)\A(?:
            \s+
            | <!DOCTYPE\ html>
            | </?(?:html|head|body|script)>
            | </(?:div|a)>
            | <div\s+class="reauth-tab-item(?:\s+reauth-tab-active)?"
                \s+data-type="(?:3|8)"\s+data-name="(?:微信扫码|短信验证码)"\s*>
            | <a\s+class="reauth-tab-more-item"\s+id="11"\s+data-name="邮箱验证码"\s*>
            | var\s+reAuthParams\s*=\s*
            | "reAuthOtpDec2"\s*:\s*"<span\ style=color:\#FF7D00\ class="synthetic">OTP</span>"
            | "service"\s*:\s*null
            | "reAuthType"\s*:\s*"(?:2|8)"
            | "isMultifactor"\s*:\s*"true"
            | "reAuthUserId"\s*:\s*"TESTUSER0001"
            | "pwdEncryptSalt"\s*:\s*"TestSalt00000000"
            | [\{\},;]
        )*\z"##,
    )
    .expect("valid fixture privacy policy");
    allowed.is_match(html)
}

#[test]
fn fixtures_contain_only_reviewed_synthetic_data() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut checked = 0;
    for entry in fs::read_dir(root).expect("read fixture directory") {
        let entry = entry.expect("read fixture entry");
        // New files/directories need their own reviewed policy. Reject symlinks
        // too, so the check cannot silently skip a capture or follow one outside.
        assert!(entry.file_type().expect("fixture type").is_file());
        assert_eq!(entry.file_name(), "reauth-page.html", "unreviewed fixture");
        let html = fs::read_to_string(entry.path()).expect("read fixture");
        assert!(
            reauth_fixture_is_synthetic(&html),
            "fixture contains unreviewed content; use minimal synthetic data (see docs/testing.md)"
        );
        checked += 1;
    }
    assert_eq!(checked, 1, "expected the reviewed reauth fixture");
}

#[test]
fn policy_rejects_identity_contact_credentials_and_user_content() {
    let fixture = include_str!("fixtures/reauth-page.html");
    assert!(reauth_fixture_is_synthetic(fixture));

    // All attack samples are invented. Do not retain the original leaked data
    // in regression tests, assertions or failure messages.
    for value in [
        "Example Person",
        "示例姓名",
        "209900000001",
        "2099XXXXXXXX",
        "+1-202-555-0100",
        "+1-202-***-0100",
        "student@example.invalid",
        r"\u793a\u4f8b",
        "&#31034;&#20363;",
    ] {
        assert!(!reauth_fixture_is_synthetic(
            &fixture.replace("TESTUSER0001", value)
        ));
    }
    assert!(!reauth_fixture_is_synthetic(
        &fixture.replace("TestSalt00000000", "unreviewed-salt")
    ));

    for field in [
        r#""displayName": "Example Person","#,
        r#""phone": "+1-202-555-0100","#,
        r#""email": "student@example.invalid","#,
        r#""password": "synthetic-password","#,
        r#""Cookie": "CASTGC=synthetic-cookie","#,
        r#""token": "synthetic-token","#,
        r#""ticket": "ST-synthetic-ticket","#,
        r#""userContent": "synthetic private message","#,
    ] {
        assert!(!reauth_fixture_is_synthetic(&fixture.replace(
            "\"service\": null",
            &format!("{field}\"service\": null")
        )));
    }

    for content in [
        "<span>Example Person</span>",
        "<!-- synthetic private message -->",
        "Cookie: CASTGC=synthetic-cookie",
        "Set-Cookie: happyVoyage=synthetic-cookie",
        "Authorization: Bearer synthetic-token",
        r#"<a href="https://example.invalid/?ticket=ST-synthetic-ticket"></a>"#,
        r#"<div class="reauth-tab-item" data-type="3" data-name="Example Person"></div>"#,
        r#"<div class="reauth-tab-item" data-type="3" data-name="短信验证码" title="Example Person"></div>"#,
    ] {
        assert!(!reauth_fixture_is_synthetic(&format!("{fixture}{content}")));
    }
}
