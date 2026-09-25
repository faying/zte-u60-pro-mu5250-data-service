//! 短信事件监听（docs/STATE_V2.md V2-31）。
//!
//! datad 起一个长期运行的 `ubus listen zwrt_wms_status_event` 子进程，逐行读它的输出；
//! 看到这个事件就（300 ms 去抖后）让短信读取「立即读」、唤醒执行者尽快开始一轮。
//!
//! - 这是**订阅**，不发任何请求（没有 `ubus call`、不经执行者后端），所以不违反「执行者单一在途」（V2-24/V2-28）：
//!   短信本身仍由执行者在它的采集轮里读。
//! - 只用 cli 方式（子进程），`ZWRT_DATAD_UBUS=socket` 时也一样：socket 后端在 Gate 0 之前不上机，
//!   直连 ubusd 的订阅（WATCH/SUBSCRIBE）以后再做。
//! - 子进程退出（或起不来）就退避重启：1 秒起、每次翻倍、最多 30 秒；连续跑满 60 秒算正常，退避回到 1 秒。
//!   日志只在第一次失败和退避到顶时各写一行，不刷屏。
//! - `ZWRT_DATAD_SMS_LISTEN=0` 关闭（默认开）。

use super::backend::cli_bin;
use std::{process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::mpsc,
    time::{Instant, sleep},
};

pub const EVENT: &str = "zwrt_wms_status_event";
pub const ENV_ENABLE: &str = "ZWRT_DATAD_SMS_LISTEN";
/// 短时间多次事件合并成一次。
pub const DEBOUNCE: Duration = Duration::from_millis(300);
pub const BACKOFF_MIN: Duration = Duration::from_secs(1);
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// 子进程连续跑满这么久，退避回到最小值。
const HEALTHY_RUN: Duration = Duration::from_secs(60);

/// `ZWRT_DATAD_SMS_LISTEN` 的值：只有 `0` 关闭。
pub fn enabled_from(value: Option<&str>) -> bool {
    value.map(str::trim) != Some("0")
}

/// `ubus listen` 的一行是不是短信事件：`{ "zwrt_wms_status_event": {...} }`。
pub fn is_event(line: &str) -> bool {
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(serde_json::Value::Object(m)) => m.contains_key(EVENT),
        _ => false,
    }
}

#[derive(Clone)]
pub struct Options {
    pub program: String,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub debounce: Duration,
}

impl Options {
    pub fn from_env() -> Self {
        Self {
            program: cli_bin(),
            backoff_min: BACKOFF_MIN,
            backoff_max: BACKOFF_MAX,
            debounce: DEBOUNCE,
        }
    }
}

/// 按环境变量起监听；关闭时返回 `None`、不起子进程。`fire` 在去抖后调用。
pub fn spawn_if_enabled(
    value: Option<&str>,
    opts: Options,
    fire: Arc<dyn Fn() + Send + Sync>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !enabled_from(value) {
        eprintln!("zwrt-datad: {ENV_ENABLE}=0，不监听 {EVENT}");
        return None;
    }
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(debounce(rx, opts.debounce, fire));
    Some(tokio::spawn(supervise(opts, tx)))
}

/// 去抖：收到第一个事件后等 `window`，期间再来的都并进这一次，然后调一次 `fire`。
pub async fn debounce(
    mut rx: mpsc::Receiver<()>,
    window: Duration,
    fire: Arc<dyn Fn() + Send + Sync>,
) {
    while rx.recv().await.is_some() {
        sleep(window).await;
        while rx.try_recv().is_ok() {}
        fire();
    }
}

/// 一直跑：起子进程、逐行读、退出就退避重启。
pub async fn supervise(opts: Options, tx: mpsc::Sender<()>) {
    let mut backoff = opts.backoff_min;
    let mut failing = false;
    loop {
        let started = Instant::now();
        let outcome = run_once(&opts.program, &tx).await;
        if tx.is_closed() {
            return;
        }
        if started.elapsed() >= HEALTHY_RUN {
            backoff = opts.backoff_min;
            failing = false;
        }
        if !failing {
            eprintln!("zwrt-datad: ubus listen {EVENT} 退出（{outcome}），{backoff:?} 后重启");
            failing = true;
        }
        sleep(backoff).await;
        let next = (backoff * 2).min(opts.backoff_max);
        if next == opts.backoff_max && backoff != opts.backoff_max {
            eprintln!(
                "zwrt-datad: ubus listen {EVENT} 反复退出（{outcome}），之后每 {next:?} 重试"
            );
        }
        backoff = next;
    }
}

/// 跑一次子进程直到它退出；返回退出原因（写日志用）。
async fn run_once(program: &str, tx: &mpsc::Sender<()>) -> String {
    let child = Command::new(program)
        .args(["listen", EVENT])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return format!("起不来：{e}"),
    };
    let Some(stdout) = child.stdout.take() else {
        return "没有 stdout".into();
    };
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if is_event(&line) && tx.send(()).await.is_err() {
                    return "datad 退出".into();
                }
            }
            Ok(None) => break,
            Err(e) => {
                let _ = child.kill().await;
                return format!("读取出错：{e}");
            }
        }
    }
    match child.wait().await {
        Ok(s) => format!("{s}"),
        Err(e) => format!("{e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn counter() -> (Arc<AtomicUsize>, Arc<dyn Fn() + Send + Sync>) {
        let n = Arc::new(AtomicUsize::new(0));
        let m = n.clone();
        (
            n,
            Arc::new(move || {
                m.fetch_add(1, Ordering::SeqCst);
            }),
        )
    }

    #[test]
    fn sms_listen_event_line_parsed() {
        assert!(is_event(r#"{ "zwrt_wms_status_event": { "sms_new": 1 } }"#));
        assert!(is_event(r#"{"zwrt_wms_status_event":{}}"#));
        assert!(!is_event(
            r#"{ "other_event": { "zwrt_wms_status_event": 1 } }"#
        ));
        assert!(!is_event("zwrt_wms_status_event"));
        assert!(!is_event(""));
    }

    /// V2-31：短时间多次事件只触发一次读取。
    #[tokio::test(start_paused = true)]
    async fn sms_events_coalesced() {
        let (n, fire) = counter();
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(debounce(rx, DEBOUNCE, fire));
        for _ in 0..5 {
            tx.send(()).await.unwrap();
            sleep(Duration::from_millis(50)).await;
        }
        sleep(Duration::from_millis(400)).await;
        assert_eq!(n.load(Ordering::SeqCst), 1, "300 ms 内 5 个事件合并成 1 次");
        tx.send(()).await.unwrap();
        sleep(Duration::from_millis(400)).await;
        assert_eq!(n.load(Ordering::SeqCst), 2, "窗口过后的新事件再触发一次");
    }

    /// V2-31：`ZWRT_DATAD_SMS_LISTEN=0` 不起监听。
    #[tokio::test]
    async fn sms_listen_disabled_by_env() {
        assert!(enabled_from(None));
        assert!(enabled_from(Some("1")));
        assert!(!enabled_from(Some("0")));
        let (_, fire) = counter();
        let opts = Options {
            program: "/nonexistent/ubus".into(),
            ..Options::from_env()
        };
        assert!(spawn_if_enabled(Some("0"), opts, fire).is_none());
    }

    fn mock_script() -> String {
        format!("{}/../tests/mock_ubus.sh", env!("CARGO_MANIFEST_DIR"))
    }

    /// V2-31：监听子进程挂掉会退避重启，重启后的事件照样送到。
    #[tokio::test]
    async fn sms_listener_restarts_after_exit() {
        let dir = std::env::temp_dir().join(format!("datad-listen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("listen.log");
        let events = dir.join("events");
        let _ = std::fs::remove_file(&log);
        std::fs::write(&events, "{ \"zwrt_wms_status_event\": {} }\n").unwrap();
        // 每个子进程读完已有的事件后约 0.2 秒退出。环境变量只给这个测试的 mock 用。
        let script = dir.join("ubus");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nMOCK_LISTEN_LOG='{}' MOCK_LISTEN_EVENTS_FILE='{}' MOCK_LISTEN_EXIT_AFTER=0 exec sh '{}' \"$@\"\n",
                log.display(),
                events.display(),
                mock_script()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        let opts = Options {
            program: script.display().to_string(),
            backoff_min: Duration::from_millis(50),
            backoff_max: Duration::from_millis(200),
            debounce: DEBOUNCE,
        };
        let h = tokio::spawn(supervise(opts, tx));
        let mut got = 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while got < 3 && std::time::Instant::now() < deadline {
            if tokio::time::timeout(Duration::from_millis(500), rx.recv())
                .await
                .ok()
                .flatten()
                .is_some()
            {
                got += 1;
            }
        }
        h.abort();
        let starts = std::fs::read_to_string(&log).unwrap_or_default();
        let starts: Vec<_> = starts.lines().collect();
        assert!(starts.len() >= 3, "至少重启两次：{starts:?}");
        assert!(starts.iter().all(|l| *l == format!("listen {EVENT}")));
        assert!(got >= 3, "每个子进程的事件都送到了：{got}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
