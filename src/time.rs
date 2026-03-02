use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use std::sync::{LazyLock, RwLock};
use tracing::error;

pub const DEFAULT_TIMEZONE: &str = "Asia/Shanghai";

static APP_TIMEZONE: LazyLock<RwLock<Tz>> =
    LazyLock::new(|| RwLock::new(chrono_tz::Asia::Shanghai));

#[cfg(test)]
pub static TEST_TIME_MUTEX: LazyLock<std::sync::Mutex<()>> =
    LazyLock::new(|| std::sync::Mutex::new(()));

pub fn sanitize_for_log(value: &str) -> String {
    value.chars().flat_map(|c| c.escape_default()).collect()
}

pub fn set_timezone(timezone: &str) -> Result<(), String> {
    let tz_name = timezone.trim();
    if tz_name.is_empty() {
        return Err("Timezone cannot be empty".to_string());
    }
    let safe_name = sanitize_for_log(tz_name);

    let parsed = tz_name.parse::<Tz>().map_err(|_| {
        format!(
            "Invalid timezone '{}'. Please use an IANA timezone name (e.g. Asia/Shanghai)",
            safe_name
        )
    })?;

    let mut guard = APP_TIMEZONE
        .write()
        .map_err(|_| "Failed to acquire timezone write lock".to_string())?;
    *guard = parsed;
    Ok(())
}

pub fn current_timezone() -> Tz {
    match APP_TIMEZONE.read() {
        Ok(tz) => *tz,
        Err(_) => {
            error!(
                "Failed to acquire timezone read lock, fallback to {}",
                DEFAULT_TIMEZONE
            );
            chrono_tz::Asia::Shanghai
        }
    }
}

pub fn current_timezone_name() -> String {
    current_timezone().to_string()
}

pub fn now() -> DateTime<Tz> {
    Utc::now().with_timezone(&current_timezone())
}

pub fn now_rfc3339() -> String {
    now().to_rfc3339()
}

pub fn now_display() -> String {
    now().format("%Y-%m-%d %H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_timezone_accepts_valid_iana_name() {
        let _guard = TEST_TIME_MUTEX.lock().expect("lock test mutex");
        let original = current_timezone().to_string();

        set_timezone("UTC").expect("set timezone to UTC");
        assert_eq!(current_timezone_name(), "UTC");

        set_timezone(&original).expect("restore original timezone");
    }

    #[test]
    fn set_timezone_rejects_invalid_name() {
        let _guard = TEST_TIME_MUTEX.lock().expect("lock test mutex");
        let original = current_timezone().to_string();

        let err = set_timezone("Not/A-Real-Timezone").expect_err("invalid timezone should fail");
        assert!(err.contains("Invalid timezone"));
        assert_eq!(current_timezone_name(), original);
    }

    #[test]
    fn now_rfc3339_contains_offset() {
        let _guard = TEST_TIME_MUTEX.lock().expect("lock test mutex");
        let original = current_timezone().to_string();

        set_timezone("Asia/Shanghai").expect("set timezone to Asia/Shanghai");
        let timestamp = now_rfc3339();
        let parsed = chrono::DateTime::parse_from_rfc3339(&timestamp).expect("parse rfc3339");
        assert_eq!(parsed.offset().local_minus_utc(), 8 * 3600);

        set_timezone(&original).expect("restore original timezone");
    }
}
