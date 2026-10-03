//! E4 写操作层（manager `docs/designs/write-op-layer.md`）：datad 里的通用事务引擎。
//!
//! - `txn`：事务状态机（纯逻辑）。
//! - `spec`：动作描述表（T2 只有网络模式）和安全类写的判断。
//! - `pending`：`pending.json` 落盘和 `takeover` 标记。
//! - `engine`：锁、覆盖/插队、确认驱动、续跑、旧请求队列。
//!
//! 环境变量：`ZWRT_DATAD_OPS_DIR`（落盘目录，默认 `/data/u60-ops`，空 = 不落盘）、
//! `ZWRT_DATAD_ROLLBACK`（`1` 才打开自动退回，D30）、`ZWRT_DATAD_OP_POLL_MS`、
//! `ZWRT_DATAD_LEGACY_TTL_MS`、各动作的 `deadline_env`；时钟 `ZWRT_DATAD_UPTIME_PATH`（默认 `/proc/uptime`）。

pub mod engine;
pub mod pending;
pub mod spec;
pub mod txn;

use crate::executor::Executor;
use serde_json::{Value, json};
use spec::{NETWORK_MODE, Spec};
use std::{path::PathBuf, time::Instant};
use txn::{Reading, SimId};

pub use engine::{Config, Engine, LegacyGate, Request, Submit};
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

fn sim_id(sim: &Value) -> SimId {
    SimId {
        iccid: text(sim, "sim_iccid"),
        slot: text(sim, "current_sim_slot").parse().unwrap_or(0),
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
                })
            }
            other => Err(format!("no reader for {other}")),
        }
    }

    async fn write(&self, spec: &'static Spec, value: &str) -> Result<Value, String> {
        let r = match spec.item {
            NETWORK_MODE => {
                crate::state::ubus(
                    "zte_nwinfo_api",
                    "nwinfo_set_netselect",
                    json!({"net_select": value}),
                )
                .await
            }
            other => Err(format!("no writer for {other}")),
        };
        // 和 `/control` 一样：写过之后慢数据缓存作废，全部块下一轮立即读。
        crate::state::invalidate_cache();
        if r.is_ok() {
            self.exec.mark_immediate(None);
        }
        r
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
