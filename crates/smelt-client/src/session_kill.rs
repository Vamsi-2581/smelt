//! 关会话（终端、ACP）时发给运行时的 kill。UI 回调不能在主线程等回执：远程运行时要经
//! iroh 拨号，离线时要等到超时；ACP 的 kill 还要等 provider 退出。所以 kill 放专用线程，
//! 这里记着还在路上的有几个，App 退出时在收尾时间里等它们发完，不悄悄丢掉。

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

static IN_FLIGHT: Mutex<usize> = Mutex::new(0);
static SETTLED: Condvar = Condvar::new();

/// 在专用线程里跑一次 kill。连线程都起不来时只记一笔，不在调用线程同步回退：资源紧张时
/// 回退反而会把 UI 卡住。
pub fn spawn(name: &str, kill: impl FnOnce() + Send + 'static) {
    *IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner()) += 1;
    let spawned = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            kill();
            settle();
        });
    if let Err(error) = spawned {
        settle();
        eprintln!("[session] 无法启动会话关闭线程：{error}");
    }
}

fn settle() {
    let mut in_flight = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    *in_flight -= 1;
    if *in_flight == 0 {
        SETTLED.notify_all();
    }
}

/// 等在路上的 kill 都发完，最多等 `timeout`。都发完了返回 true。阻塞，不能在主线程调。
pub fn wait_settled(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut in_flight = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    while *in_flight > 0 {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        in_flight = SETTLED
            .wait_timeout(in_flight, left)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
    true
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    /// 调度不等 kill 做完；退出时能等到它做完。
    #[test]
    fn kill_runs_off_the_caller_and_quit_can_wait_for_it() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        spawn("test-session-kill", move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("kill 应在后台开始执行");
        assert!(
            !wait_settled(Duration::from_millis(50)),
            "kill 还没做完，不能报已发完"
        );
        release_tx.send(()).unwrap();
        assert!(wait_settled(Duration::from_secs(1)), "kill 做完后应等得到");
    }
}
