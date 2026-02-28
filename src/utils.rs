use std::future::Future;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, warn};

pub async fn retry<F, Fut, T, E>(
    mut operation: F,
    max_retries: usize,
    initial_delay: Duration,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    debug!(
        "Starting retry operation with max_retries={}, initial_delay={:?}",
        max_retries, initial_delay
    );
    let mut delay = initial_delay;
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
                delay *= 2; // Exponential backoff
                debug!("Next retry delay: {:?}", delay);
            }
        }
    }
    unreachable!()
}
