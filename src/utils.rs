use std::future::Future;
use std::time::Duration;
use tokio::time::sleep;
use tracing::warn;

pub async fn retry<F, Fut, T, E>(
    mut operation: F,
    max_retries: usize,
    initial_delay: Duration,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let mut delay = initial_delay;
    for i in 0..max_retries {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(e) => {
                if i == max_retries - 1 {
                    return Err(e);
                }
                warn!(
                    "Operation failed (attempt {}/{}): {}. Retrying in {:?}...",
                    i + 1,
                    max_retries,
                    e,
                    delay
                );
                sleep(delay).await;
                delay *= 2; // Exponential backoff
            }
        }
    }
    unreachable!()
}
