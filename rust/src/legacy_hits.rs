//! 旧接口访问计数（u60-platform「现在就能开工」第 3 条）：删 `/state`、`/events` 和没人用的 `/control`
//! 动作之前，先记下谁还在调。读数在 `GET /debug/legacy-hits`，不进 `/state`、`/capabilities`、`/v2`
//! （golden 和 /v2 契约都不变）。
//!
//! - 计数存在数据目录的 `legacy-hits.json`（设备上 `/data/zwrt-datad/`），datad 重启、换版本、
//!   整机重启都接着数；`since` 是第一次开始数的时间，看「零」的时候看它有多久。
//! - 写盘：某个键第一次出现时马上写，之后最多每分钟一次（目标是零次，写得很少）。
//! - 调用者：本机连接按对端端口在 `/proc/net/tcp*` 找 socket inode，再扫 `/proc/*/fd` 找进程，
//!   记 `comm`、pid 和父进程命令行；同一个键 5 秒内只查一次，每个键留最近 4 个不同的调用者。
//!   局域网连接只记 IP。查不到（进程已经退出、不在 Linux 上）就写 `?`。
//! - `/control` 按动作分键，带 `source` 的另算一个键（`control:<动作>+source`）；不同的键最多 256 个，
//!   多出来的都记到 `control:(other)`。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_KEYS: usize = 256;
const MAX_CALLERS: usize = 4;
const RESOLVE_EVERY: Duration = Duration::from_secs(5);
const SAVE_EVERY: Duration = Duration::from_secs(60);

#[derive(Clone, Default, Serialize, Deserialize)]
struct Hit {
    count: u64,
    first: i64,
    last: i64,
    #[serde(default)]
    callers: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    since: i64,
    #[serde(default)]
    hits: BTreeMap<String, Hit>,
}

struct Book {
    saved: Saved,
    path: PathBuf,
    dirty: bool,
    last_save: Option<Instant>,
    last_resolve: BTreeMap<String, Instant>,
}

static DIR: OnceLock<PathBuf> = OnceLock::new();

fn book() -> &'static Mutex<Book> {
    static BOOK: OnceLock<Mutex<Book>> = OnceLock::new();
    BOOK.get_or_init(|| {
        let path = DIR
            .get()
            .cloned()
            .unwrap_or_else(|| PathBuf::from("/data/zwrt-datad"))
            .join("legacy-hits.json");
        let loaded = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Saved>(&bytes).ok())
            .filter(|saved| saved.since > 0);
        let dirty = loaded.is_none();
        let saved = loaded.unwrap_or_else(|| Saved {
            since: now(),
            hits: BTreeMap::new(),
        });
        Mutex::new(Book {
            saved,
            path,
            dirty,
            last_save: None,
            last_resolve: BTreeMap::new(),
        })
    })
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 启动时、开始监听之前调一次：读出旧计数；文件还没有就马上写一份，`since` 从现在算。
pub fn init(data_dir: &std::path::Path) {
    let _ = DIR.set(data_dir.to_owned());
    let first = book().lock().map(|b| b.dirty).unwrap_or(false);
    if first {
        flush(true);
    }
}

/// 一次请求的键：`/state`、`/events` 原样；`/control` 用 [`control_key`]。
pub fn control_key(action: &str, has_source: bool) -> String {
    if has_source {
        format!("control:{action}+source")
    } else {
        format!("control:{action}")
    }
}

/// 记一次访问。要查调用者时在阻塞线程里扫 /proc，并等它查完再回复：本机 `/state` 调用者拿完就断，
/// 回复之后 socket 就没了。同一个键 5 秒内只查一次，所以只有第一次慢几毫秒。
pub async fn hit(key: &str, peer: Option<SocketAddr>) {
    let (key, new_key, resolve) = {
        let Ok(mut b) = book().lock() else { return };
        let key = if b.saved.hits.contains_key(key) || b.saved.hits.len() < MAX_KEYS {
            key.to_owned()
        } else {
            "control:(other)".to_owned()
        };
        let t = now();
        let new_key = !b.saved.hits.contains_key(&key);
        let entry = b.saved.hits.entry(key.clone()).or_default();
        if entry.count == 0 {
            entry.first = t;
        }
        entry.count += 1;
        entry.last = t;
        b.dirty = true;
        let due = peer.is_some()
            && b.last_resolve
                .get(&key)
                .is_none_or(|at| at.elapsed() >= RESOLVE_EVERY);
        if due {
            b.last_resolve.insert(key.clone(), Instant::now());
        }
        (key, new_key, due)
    };
    if !resolve && !new_key {
        // 不查也不急着写：每分钟的 flush 会带上。
        return;
    }
    let run = move || {
        if let Some(peer) = peer.filter(|_| resolve) {
            let caller = describe(peer);
            if let Ok(mut b) = book().lock()
                && let Some(entry) = b.saved.hits.get_mut(&key)
            {
                entry.callers.retain(|c| c != &caller);
                entry.callers.insert(0, caller);
                entry.callers.truncate(MAX_CALLERS);
            }
        }
        flush(new_key);
    };
    let _ = tokio::task::spawn_blocking(run).await;
}

/// 有改动就写盘：`force` 马上写，否则距上次至少一分钟。写失败下次再试。
pub fn flush(force: bool) {
    let (path, bytes) = {
        let Ok(mut b) = book().lock() else { return };
        if !b.dirty {
            return;
        }
        if !force && b.last_save.is_some_and(|at| at.elapsed() < SAVE_EVERY) {
            return;
        }
        let Ok(bytes) = serde_json::to_vec_pretty(&b.saved) else {
            return;
        };
        b.dirty = false;
        b.last_save = Some(Instant::now());
        (b.path.clone(), bytes)
    };
    if write_atomic(&path, &bytes).is_err()
        && let Ok(mut b) = book().lock()
    {
        b.dirty = true;
    }
}

fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// `GET /debug/legacy-hits` 的回复。
pub fn report() -> Value {
    let Ok(b) = book().lock() else {
        return json!({"ok": false});
    };
    json!({
        "ok": true,
        "since": b.saved.since,
        "now": now(),
        "hits": b.saved.hits.iter().map(|(k, h)| (k.clone(), json!({
            "count": h.count, "first": h.first, "last": h.last, "callers": h.callers,
        }))).collect::<serde_json::Map<_, _>>(),
    })
}

/// 调用者的一句描述。
fn describe(peer: SocketAddr) -> String {
    let ip = match peer.ip() {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        ip => ip,
    };
    if !ip.is_loopback() {
        return format!("lan {ip}");
    }
    match find_pid(peer.port()) {
        Some(pid) => {
            let comm = read_trim(&format!("/proc/{pid}/comm"));
            let ppid = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| parse_ppid(&stat));
            let parent = ppid.map(cmdline).unwrap_or_else(|| "?".into());
            format!("{comm} pid {pid} parent: {parent}")
        }
        None => "?".into(),
    }
}

fn read_trim(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|_| "?".into())
}

fn cmdline(pid: u32) -> String {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let mut text: String = String::from_utf8_lossy(&raw)
        .split('\0')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        text = read_trim(&format!("/proc/{pid}/comm"));
    }
    if text.chars().count() > 80 {
        text = text.chars().take(80).collect::<String>() + "…";
    }
    text
}

/// `/proc/<pid>/stat` 的第 4 个字段；`comm` 在括号里、可能带空格，从最后一个 `)` 后面数。
fn parse_ppid(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// 本机这一端用 `port` 的 TCP 连接属于哪个进程。
fn find_pid(port: u16) -> Option<u32> {
    let inode = ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .find_map(|table| socket_inode(&table, port))?;
    let wanted = format!("socket:[{inode}]");
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if std::fs::read_link(fd.path()).is_ok_and(|l| l.as_os_str() == wanted.as_str()) {
                return Some(pid);
            }
        }
    }
    None
}

/// `/proc/net/tcp*` 里本地端口是 `port` 的那一行的 inode（第 10 列）。
fn socket_inode(table: &str, port: u16) -> Option<u64> {
    let want = format!(":{port:04X}");
    table.lines().skip(1).find_map(|line| {
        let cols: Vec<&str> = line.split_whitespace().collect();
        let local = cols.get(1)?;
        if !local.ends_with(&want) {
            return None;
        }
        cols.get(9)?.parse::<u64>().ok().filter(|inode| *inode != 0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ppid_skips_comm_with_spaces_and_parens() {
        assert_eq!(parse_ppid("123 (a b) c) S 45 1 1"), Some(45));
        assert_eq!(parse_ppid("123 (sh) S 7 7 7"), Some(7));
    }

    #[test]
    fn inode_matches_local_port_only() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:24F4 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 111 1 0 100 0 0 10 0\n\
   1: 0100007F:C350 0100007F:24F4 01 00000000:00000000 00:00000000 00000000     0        0 222 1 0 20 4 30 10 -1\n\
   2: 0100007F:24F4 0100007F:C350 01 00000000:00000000 00:00000000 00000000     0        0 333 1 0 20 4 30 10 -1\n";
        assert_eq!(socket_inode(table, 0xC350), Some(222));
        assert_eq!(socket_inode(table, 0x24F4), Some(111));
        assert_eq!(socket_inode(table, 1), None);
    }

    #[test]
    fn control_keys_split_by_source() {
        assert_eq!(control_key("wifi.set", false), "control:wifi.set");
        assert_eq!(control_key("wifi.set", true), "control:wifi.set+source");
    }
}
