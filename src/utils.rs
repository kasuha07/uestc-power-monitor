use std::future::Future;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, warn};

/// 未显式指定上限时的退避封顶，避免指数退避增长到不可接受的等待时间。
const DEFAULT_MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

pub async fn retry<F, Fut, T, E>(
    operation: F,
    max_retries: usize,
    initial_delay: Duration,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    retry_with_backoff(
        operation,
        max_retries,
        initial_delay,
        DEFAULT_MAX_RETRY_DELAY,
    )
    .await
}

pub async fn retry_with_backoff<F, Fut, T, E>(
    mut operation: F,
    max_retries: usize,
    initial_delay: Duration,
    max_delay: Duration,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let max_retries = max_retries.max(1);
    let mut delay = initial_delay.min(max_delay);
    debug!(
        "Starting retry operation with max_retries={}, initial_delay={:?}, max_delay={:?}",
        max_retries, delay, max_delay
    );

    for i in 0..max_retries {
        debug!("Retry attempt {}/{}", i + 1, max_retries);
        match operation().await {
            Ok(value) => {
                debug!("Operation succeeded on attempt {}/{}", i + 1, max_retries);
                return Ok(value);
            }
            Err(e) => {
                if i == max_retries - 1 {
                    debug!("All retry attempts exhausted");
                    return Err(e);
                }
                warn!(
                    "Operation failed (attempt {}/{}): request error (details redacted). Retrying in {:?}...",
                    i + 1,
                    max_retries,
                    delay
                );
                sleep(delay).await;
                delay = delay.checked_mul(2).unwrap_or(max_delay).min(max_delay);
                debug!("Next retry delay: {:?}", delay);
            }
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(start_paused = true)]
    async fn succeeds_without_sleeping_when_the_first_attempt_works() {
        let start = tokio::time::Instant::now();
        let attempts = AtomicUsize::new(0);

        let result: Result<u32, &str> = retry(
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                async { Ok(7) }
            },
            3,
            Duration::from_secs(5),
        )
        .await;

        assert_eq!(result, Ok(7));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_max_retries_and_returns_the_last_error() {
        let attempts = AtomicUsize::new(0);

        let result: Result<(), &str> = retry(
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                async { Err("nope") }
            },
            3,
            Duration::from_secs(5),
        )
        .await;

        assert_eq!(result, Err("nope"));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_is_capped_at_the_explicit_max_delay() {
        let start = tokio::time::Instant::now();

        let result: Result<(), &str> = retry_with_backoff(
            || async { Err("nope") },
            4,
            Duration::from_secs(10),
            Duration::from_secs(15),
        )
        .await;

        assert!(result.is_err());
        // 10s, then capped at 15s twice — not 10s + 20s + 40s.
        assert_eq!(start.elapsed(), Duration::from_secs(40));
    }

    #[tokio::test(start_paused = true)]
    async fn default_backoff_is_capped_instead_of_growing_without_bound() {
        let start = tokio::time::Instant::now();

        let result: Result<(), &str> =
            retry(|| async { Err("nope") }, 5, Duration::from_secs(30)).await;

        assert!(result.is_err());
        // 30s, 60s, 60s, 60s — uncapped exponential would have waited 450s.
        assert_eq!(start.elapsed(), Duration::from_secs(210));
        assert_eq!(DEFAULT_MAX_RETRY_DELAY, Duration::from_secs(60));
    }
}
