//! E4 写操作层（manager `docs/designs/write-op-layer.md`）：datad 里的通用事务引擎。
//!
//! - `txn`：事务状态机（纯逻辑）。
//! - `spec`：动作描述表（T2 只有网络模式）和安全类写的判断。
//! - `probe`：确认用的 DNS 探测，绑定蜂窝接口（T4）。
//! - `pending`：`pending.json` 落盘和 `takeover` 标记。
//! - `record`：流水账 `journal.jsonl` 和 `owners.json`，单一写者（T5）。
//! - `engine`：锁、覆盖/插队、确认驱动、续跑、旧请求队列。
//! - `ui`：给界面的文字和标志（T13：状态文案表、三行进度、撤销、首页「进行中」）。
//! - `write_lock`：跨进程写锁（flock，和应急直写脚本互斥，D29）。
//!
//! 环境变量：`ZWRT_DATAD_OPS_DIR`（落盘目录，默认 `/data/u60-ops`，空 = 不落盘）、
//! `ZWRT_DATAD_ROLLBACK`（`1` 才打开自动退回，D30）、`ZWRT_DATAD_OP_POLL_MS`、
//! `ZWRT_DATAD_LEGACY_TTL_MS`、各动作的 `deadline_env`、`ZWRT_DATAD_WRITE_LOCK`；时钟 `ZWRT_DATAD_UPTIME_PATH`（默认 `/proc/uptime`）。

pub mod engine;
pub mod journal_view;
pub mod pending;
pub mod probe;
pub mod record;
pub mod spec;
pub mod txn;
pub mod ui;
pub mod write_lock;

use crate::executor::Executor;
use serde_json::{Value, json};
use spec::{NETWORK_MODE, Spec};
use std::{path::PathBuf, time::Instant};
use txn::{Conn, DataPath, ProbeTarget, Reading, SimId};

pub use engine::{Config, Engine, LegacyGate, Request, Submit, WriteError};
pub use txn::Source;

pub fn ops_dir() -> Option<PathBuf> {
    match std::env::var("ZWRT_DATAD_OPS_DIR") {
        Ok(v) if v.is_empty() => None,
        Ok(v) => Some(v.into()),
        Err(_) => Some("/data/u60-ops".into()),
    }
}

/// 真机：读写都经 `state::ubus`（执行者里的短任务）。
pub struct UbusDevice {
    exec: Executor,
    started: Instant,
}

impl UbusDevice {
    pub fn new(exec: Executor) -> Self {
        Self {
            exec,
            started: Instant::now(),
        }
    }
}

fn text(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// 已注册：`network_type` 有值，且不是无服务/受限/紧急（和首页 `screen::rat_of` 认 None 的写法一致）。
fn registered(network_type: &str) -> bool {
    let u = network_type.trim().to_ascii_uppercase();
    !u.is_empty()
        && ![
            "LIMIT",
            "EMERGENCY",
            "NO_SERVICE",
            "NOSERVICE",
            "NO SERVICE",
        ]
        .iter()
        .any(|w| u.contains(w))
}

/// 和首页（`screen.rs`）同一个判断：空、`Home`、`home`、`0` 都不算漫游；空的另外算「不知道」。
fn roaming(net: &Value) -> Option<bool> {
    let r = text(net, "simcard_roam");
    (!r.is_empty()).then(|| r != "Home" && r != "home" && r != "0")
}

/// `connect_status` 是已连接：`ipv4_connected`、`ipv4_ipv6_connected` 这类；`disconnected` 不算
/// （和 agent netwatch 的 `wan_connected` 一致）。
fn connected(status: &str) -> bool {
    status.ends_with("connected") && !status.contains("disconnect")
}

/// 移动数据开关。`enable` 是最近一次写进去的值，开机后默认 0，自动拨号照样连上（T12 真机，B31），
/// 所以 0 但正在连或已连上也算开着；0 且断着才算关了；状态读空不猜。
pub(crate) fn data_switch(wwan: &Value) -> Option<bool> {
    if flag(wwan, "enable")? {
        return Some(true);
    }
    let status = text(wwan, "connect_status");
    if status.is_empty() {
        return None;
    }
    Some(connected(&status) || status == "connecting")
}

fn flag(v: &Value, key: &str) -> Option<bool> {
    if let Some(b) = v.get(key).and_then(Value::as_bool) {
        return Some(b);
    }
    match text(v, key).as_str() {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}

/// 数据通路：`get_wwaniface`（开关、连接状态、接口）+ netifd 的 `zte_wan`（IPv4、连上多久、DNS）+
/// nwinfo 的漫游。`now_ms` 用来把 uptime 秒数换成连接的起点。
fn data_path(
    net: &Value,
    wwan: &Value,
    wan: &Value,
    uci_dns: Option<String>,
    now_ms: u64,
) -> DataPath {
    let expected = match (data_switch(wwan), flag(wwan, "roam_enable"), roaming(net)) {
        (Some(false), _, _) => Some(false),
        (Some(true), _, Some(false)) => Some(true),
        (Some(true), Some(roam_on), Some(true)) => Some(roam_on),
        _ => None,
    };
    let ipv4 = wan
        .get("ipv4-address")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .map(|a| text(a, "address"))
        .unwrap_or_default();
    let up_since_ms = wan
        .get("uptime")
        .and_then(Value::as_u64)
        .map(|secs| now_ms.saturating_sub(secs * 1000));
    let mut iface = text(wwan, "ipv4_dev_name");
    if iface.is_empty() {
        iface = text(wan, "l3_device");
    }
    let mut dns: Vec<String> = wan
        .get("dns-server")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if dns.is_empty() {
        // uci 里是 `'a' 'b'` 或空格分开的一串。
        dns = uci_dns
            .unwrap_or_default()
            .split(|c: char| c == '\'' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
    }
    DataPath {
        expected,
        connected: connected(&text(wwan, "connect_status")),
        conn: Conn { ipv4, up_since_ms },
        iface,
        dns,
    }
}

fn sim_id(sim: &Value) -> SimId {
    SimId {
        iccid: text(sim, "sim_iccid"),
        slot: text(sim, "current_sim_slot").parse().unwrap_or(0),
    }
}

impl UbusDevice {
    /// 读不到 `get_wwaniface` 或 `zte_wan` 就是 None（这一拍不判断数据）。
    async fn data(&self, net: &Value) -> Option<DataPath> {
        let wwan = crate::state::ubus(
            "zwrt_data",
            "get_wwaniface",
            json!({"source_module":"web","cid":1,"connect_status":""}),
        )
        .await
        .ok()?;
        let wan = crate::state::ubus("network.interface.zte_wan", "status", json!({}))
            .await
            .ok()?;
        let no_dns = wan
            .get("dns-server")
            .and_then(Value::as_array)
            .is_none_or(|a| a.is_empty());
        let uci_dns = if no_dns {
            Some(crate::state::uci_read("network.zte_wan.dns").await)
        } else {
            None
        };
        Some(data_path(
            net,
            &wwan,
            &wan,
            uci_dns,
            engine::Device::now_ms(self),
        ))
    }
}

impl engine::Device for UbusDevice {
    async fn read(&self, spec: &'static Spec) -> Result<Reading, String> {
        match spec.item {
            NETWORK_MODE => {
                let net =
                    crate::state::ubus("zte_nwinfo_api", "nwinfo_get_netinfo", json!({})).await?;
                let sim = crate::state::ubus("zwrt_zte_mdm.api", "get_sim_info", json!({})).await?;
                let value = text(&net, "net_select");
                Ok(Reading {
                    value: (!value.is_empty()).then_some(value),
                    registered: registered(&text(&net, "network_type")),
                    sim: sim_id(&sim),
                    data: self.data(&net).await,
                    probe: None,
                })
            }
            other => Err(format!("no reader for {other}")),
        }
    }

    async fn write(&self, spec: &'static Spec, value: &str) -> Result<Value, WriteError> {
        let (object, method, args) = match spec.item {
            NETWORK_MODE => (
                "zte_nwinfo_api",
                "nwinfo_set_netselect",
                json!({"net_select": value}),
            ),
            other => {
                return Err(WriteError {
                    message: format!("no writer for {other}"),
                    timed_out: false,
                });
            }
        };
        // 一个执行者任务：在执行者里拿跨进程写锁（D29）再调，锁不会和别的任务互等。
        let r = self
            .exec
            .task(async move {
                let _lock = write_lock::acquire().await;
                // nwinfo_set_netselect 成功时什么都不回（T11 真机，B31）
                crate::control::write_reply(crate::executor::call(object, method, &args).await)
            })
            .await;
        // 和 `/control` 一样：写过之后慢数据缓存作废，全部块下一轮立即读。
        crate::state::invalidate_cache();
        match r {
            Ok(v) => {
                self.exec.mark_immediate(None);
                Ok(v)
            }
            Err(e) => Err(WriteError {
                // D28：我们这边超时不代表原厂那边没做，结果未知，交给读回判断。
                timed_out: matches!(e, crate::ubus::client::UbusError::Timeout { .. }),
                message: e.to_string(),
            }),
        }
    }

    async fn probe(&self, target: &ProbeTarget) -> Result<(), String> {
        probe::dns(target).await
    }

    fn now_ms(&self) -> u64 {
        let path =
            std::env::var("ZWRT_DATAD_UPTIME_PATH").unwrap_or_else(|_| "/proc/uptime".into());
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
            .map(|secs| (secs * 1000.0) as u64)
            .unwrap_or_else(|| self.started.elapsed().as_millis() as u64)
    }

    fn boot_id(&self) -> String {
        let path = std::env::var("ZWRT_DATAD_BOOT_ID_PATH")
            .unwrap_or_else(|_| "/proc/sys/kernel/random/boot_id".into());
        std::fs::read_to_string(path)
            .map(|s| s.trim().to_owned())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_words() {
        for t in ["SA", "NSA", "LTE", "WCDMA", "ENDC"] {
            assert!(registered(t), "{t}");
        }
        for t in [
            "",
            " ",
            "No Service",
            "NO_SERVICE",
            "Limited Service",
            "EMERGENCY",
        ] {
            assert!(!registered(t), "{t}");
        }
    }

    #[test]
    fn connected_words() {
        for s in ["ipv4_connected", "ipv6_connected", "ipv4_ipv6_connected"] {
            assert!(connected(s), "{s}");
        }
        for s in ["", "disconnected", "ipv4_disconnected", "connecting"] {
            assert!(!connected(s), "{s}");
        }
    }

    fn wwan(enable: i64, roam: i64) -> Value {
        // 写 0 关数据之后读到的就是断开（T12）
        let status = if enable == 1 {
            "ipv4_ipv6_connected"
        } else {
            "disconnected"
        };
        json!({"enable":enable,"roam_enable":roam,"connect_status":status,"ipv4_dev_name":"rmnet_data0"})
    }

    #[test]
    fn data_switch_reads_a_boot_default_enable_as_on() {
        let w = |e: Value, s: &str| data_switch(&json!({"enable":e,"connect_status":s}));
        assert_eq!(w(json!(1), "disconnected"), Some(true));
        // 开机后没人写过：enable 0，自动拨号连着（10-04 真机）
        assert_eq!(w(json!(0), "ipv4_ipv6_connected"), Some(true));
        assert_eq!(w(json!(0), "connecting"), Some(true));
        assert_eq!(w(json!(0), "disconnected"), Some(false));
        assert_eq!(w(json!(0), "disconnecting"), Some(false));
        assert_eq!(w(json!(0), ""), None);
        assert_eq!(
            data_switch(&json!({"connect_status":"ipv4_connected"})),
            None
        );
    }

    fn wan() -> Value {
        json!({"uptime":30,"l3_device":"rmnet_data9","ipv4-address":[{"address":"10.1.2.3","mask":30}],"dns-server":["192.0.2.53","2001:db8::53"]})
    }

    #[test]
    fn data_is_expected_only_with_the_switches_that_allow_it() {
        let home = json!({"simcard_roam":"Home"});
        let away = json!({"simcard_roam":"Roaming"});
        let unknown = json!({});
        let e = |net: &Value, w: Value| data_path(net, &w, &wan(), None, 100_000).expected;
        assert_eq!(e(&home, wwan(1, 0)), Some(true));
        assert_eq!(e(&home, wwan(0, 1)), Some(false));
        assert_eq!(e(&away, wwan(1, 0)), Some(false));
        assert_eq!(e(&away, wwan(1, 1)), Some(true));
        assert_eq!(e(&away, wwan(0, 1)), Some(false));
        // 不知道在不在漫游：不猜。
        assert_eq!(e(&unknown, wwan(1, 0)), None);
        assert_eq!(e(&home, json!({"roam_enable":0})), None);
        // 布尔、字符串写法都认
        assert_eq!(
            e(&away, json!({"enable":true,"roam_enable":false})),
            Some(false)
        );
        assert_eq!(
            e(&away, json!({"enable":"1","roam_enable":"1"})),
            Some(true)
        );
    }

    #[test]
    fn data_path_fields() {
        let d = data_path(
            &json!({"simcard_roam":"Home"}),
            &wwan(1, 0),
            &wan(),
            None,
            100_000,
        );
        assert!(d.connected);
        assert_eq!(
            d.conn,
            Conn {
                ipv4: "10.1.2.3".into(),
                up_since_ms: Some(70_000)
            }
        );
        // 接口按 get_wwaniface 报的，不是 netifd 的 l3_device。
        assert_eq!(d.iface, "rmnet_data0");
        assert_eq!(d.dns, ["192.0.2.53", "2001:db8::53"]);
        let bare = data_path(
            &json!({}),
            &json!({"connect_status":"disconnected"}),
            &json!({"l3_device":"rmnet_data0"}),
            Some("'222.66.251.8' '116.236.159.8'".into()),
            1_000,
        );
        assert!(!bare.connected);
        assert_eq!(bare.iface, "rmnet_data0");
        assert_eq!(bare.conn, Conn::default());
        assert_eq!(bare.dns, ["222.66.251.8", "116.236.159.8"]);
    }

    #[test]
    fn sim_identity_reads_number_or_string_slot() {
        assert_eq!(
            sim_id(&json!({"sim_iccid":"8986","current_sim_slot":2})),
            SimId {
                iccid: "8986".into(),
                slot: 2
            }
        );
        assert_eq!(sim_id(&json!({"current_sim_slot":"1"})).slot, 1);
    }
}
