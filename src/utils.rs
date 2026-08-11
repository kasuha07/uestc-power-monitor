use std::future::Future;
use std::io::{self, IsTerminal};
use std::os::fd::AsRawFd;
use std::thread;
use std::time::{Duration, Instant};
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

/// 判断 stdin 是否有"活着的"交互终端（有人在前台等待输入）。
///
/// 判定规则：
/// - stdin 不是终端（管道/文件——systemd、CI、无 TTY 的 Docker 容器）→ false；
/// - 是终端但无人连接（典型：`tty: true` 的 Docker 容器后台启动、没有客户端
///   attach，PTY 窗口尺寸为 0）→ false。此时若进入交互输入，进程会永久挂起
///   在提示上，且提示行因 canonical 行缓冲连 `docker logs` 都看不到；
/// - 是终端且窗口尺寸非 0（真实终端、已 attach 的 Docker 容器）→ true。
///
/// Docker 客户端在 attach 时发送 resize，窗口尺寸约 1~2 秒内到达，因此
/// 对"是 TTY 但尺寸为 0"的情况轮询等待一小段宽限期再判定。
/// 非 Linux 平台无法查询窗口尺寸，直接退回 `is_terminal()`。
pub fn stdin_is_interactive() -> bool {
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        return false;
    }

    #[cfg(target_os = "linux")]
    {
        // 宽限期：等 Docker attach 后的 resize（或输入）到达，避免误判刚连上的终端。
        const POLL_INTERVAL: Duration = Duration::from_millis(200);
        const GRACE_PERIOD: Duration = Duration::from_secs(3);
        let start = Instant::now();
        while match tty_size(stdin.as_raw_fd()) {
            Some((rows, cols)) => rows == 0 && cols == 0,
            // ioctl 失败（罕见）按"有人"处理，退回 is_terminal() 的语义。
            None => false,
        } {
            if start.elapsed() >= GRACE_PERIOD {
                return false;
            }
            thread::sleep(POLL_INTERVAL);
        }
        return true;
    }

    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// 查询 stdin 终端的窗口尺寸 `(行, 列)`；非终端或查询失败返回 `None`。
/// 无人 attach 的 PTY（如后台启动的 tty Docker 容器）返回 `Some((0, 0))`。
#[cfg(target_os = "linux")]
fn tty_size(fd: i32) -> Option<(u16, u16)> {
    #[repr(C)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }
    // TIOCGWINSZ = 0x5413（x86_64 / aarch64 Linux 均为此值）
    const TIOCGWINSZ: u64 = 0x5413;
    let mut ws = std::mem::MaybeUninit::<Winsize>::uninit();
    let ret = unsafe { sys_ioctl(fd, TIOCGWINSZ, ws.as_mut_ptr()) };
    if ret < 0 {
        return None;
    }
    let ws = unsafe { ws.assume_init() };
    Some((ws.ws_row, ws.ws_col))
}

#[cfg(target_os = "linux")]
// 绕过 libc crate 直接调用 ioctl（第三个参数为变参）。
unsafe extern "C" {
    #[link_name = "ioctl"]
    fn sys_ioctl(fd: i32, request: u64, ...) -> i32;
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

    #[test]
    fn stdin_is_not_interactive_when_redirected() {
        // cargo test 环境下 stdin 为管道/文件，不应判定为可交互（否则会挂起等待输入）。
        assert!(!stdin_is_interactive());
    }
}
