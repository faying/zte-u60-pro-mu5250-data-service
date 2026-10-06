//! 动作描述表（write-op-layer.md「动作描述表 ActionSpec」）：哪些 `/control` 动作走事务、改的是哪一项、
//! 目标值从哪个参数来、确认时限多长。T2 先只接网络模式；其余动作照旧走 `control::execute`。
//! 读回和写怎么做在 `UbusDevice`（按 `item` 分）。

use super::txn::Confirm;
use serde_json::Value;

pub struct Spec {
    pub action: &'static str,
    /// 改的是哪一项：同一项的写才能互相覆盖，撤销、owners 也按它算。
    pub item: &'static str,
    /// 目标值所在的 `/control` 参数（字符串）。
    pub param: &'static str,
    /// 确认时限（D24 暂定值，T11 用 netwatch 统计定稿）。
    pub deadline_ms: u64,
    /// 覆盖确认时限的环境变量（毫秒）。
    pub deadline_env: &'static str,
    /// 确认规则（D34：只有 APN 要求写之后的新连接）。
    pub confirm: Confirm,
}

pub const NETWORK_MODE: &str = "network.mode";

pub static SPECS: &[Spec] = &[Spec {
    action: "network.set_mode",
    item: NETWORK_MODE,
    param: "mode",
    deadline_ms: 120_000,
    deadline_env: "ZWRT_DATAD_DEADLINE_NETWORK_MODE_MS",
    confirm: Confirm::Registered,
}];

pub fn find(action: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.action == action)
}

impl Spec {
    pub fn deadline_ms(&self) -> u64 {
        std::env::var(self.deadline_env)
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|v| *v > 0)
            .unwrap_or(self.deadline_ms)
    }

    /// 目标值；错误文字和旧接口一致（`control.rs` 的 `string()`）。
    pub fn target(&self, params: &Value) -> Result<String, String> {
        crate::control::string(params, self.param, true).map(|v| v.unwrap_or_default())
    }
}

/// D14：`cellular.set` 只把数据或漫游关掉（参数只有 `enabled`/`roaming`，且都是关）算安全类写，
/// 马上插队（新旧客户端一样）。参数名按 `/control` 的（不是 ubus 的 enable/roam_enable）。
pub fn is_safety(action: &str, params: &Value) -> bool {
    if action != "cellular.set" {
        return false;
    }
    let Some(map) = params.as_object() else {
        return false;
    };
    !map.is_empty()
        && map.keys().all(|k| k == "enabled" || k == "roaming")
        && map
            .keys()
            .all(|k| crate::control::boolean(params, k) == Ok(false))
}

/// D40：`vendor.call` 里影响上网的 (对象, 方法)：STC 小区锁、SIM PIN/PUK/网络锁。
const NETWORK_VENDOR_CALLS: &[(&str, &str)] = &[
    ("zte_nwinfo_api", "nwinfo_set_stc_white_list_par"),
    ("zte_nwinfo_api", "nwinfo_stc_cell_lock_enable"),
    ("zte_nwinfo_api", "nwinfo_stc_cell_lock_disable"),
    ("zte_nwinfo_api", "nwinfo_stc_cell_lock_reset"),
    ("zwrt_zte_mdm.api", "sim_verify_pin_puk"),
    ("zwrt_zte_mdm.api", "sim_change_pin"),
    ("zwrt_zte_mdm.api", "sim_change_pin_mode"),
    ("zwrt_zte_mdm.api", "set_simlock_nck"),
];

/// D40：事务在确认或退回中时要按来源处理的「影响上网的写」：会话期间收 409 的那些（描述表里的动作除外，
/// 同一项由引擎按覆盖处理）、回自动和重拨、`vendor.call` 的 STC 小区锁和 SIM PIN 类。
/// 搜网会话的步骤照会话规则；安全类写（[`is_safety`]）由调用方先排除，照旧插队。
pub fn affects_network(action: &str, params: &Value) -> bool {
    use super::engine::{SESSION_BLOCKS, SESSION_OPTIONAL};
    if find(action).is_some() {
        return false;
    }
    if SESSION_BLOCKS.contains(&action) || SESSION_OPTIONAL.contains(&action) {
        return true;
    }
    action == "vendor.call"
        && NETWORK_VENDOR_CALLS.iter().any(|(o, m)| {
            params.get("object").and_then(Value::as_str) == Some(o)
                && params.get("method").and_then(Value::as_str) == Some(m)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn network_affecting_writes() {
        for a in [
            "cellular.set",
            "band.set_lte",
            "band.reset",
            "cell.lock_nr",
            "apn.modify",
            "modem.online",
            "apn.set_pdp_type",
            "netselect.auto",
            "cellular.redial",
        ] {
            assert!(affects_network(a, &json!({})), "{a}");
        }
        // 描述表里的（引擎按同一项处理）、搜网步骤、别的写都不算
        for a in [
            "network.set_mode",
            "netselect.scan",
            "netselect.register",
            "sms.delete",
            "wifi.apply",
            "usb.set",
        ] {
            assert!(!affects_network(a, &json!({})), "{a}");
        }
        let v = |o: &str, m: &str| affects_network("vendor.call", &json!({"object":o,"method":m}));
        assert!(v("zte_nwinfo_api", "nwinfo_stc_cell_lock_enable"));
        assert!(v("zte_nwinfo_api", "nwinfo_set_stc_white_list_par"));
        assert!(v("zwrt_zte_mdm.api", "sim_verify_pin_puk"));
        assert!(v("zwrt_zte_mdm.api", "set_simlock_nck"));
        assert!(!v("zte_nwinfo_api", "nwinfo_start_detect_signal_quality"));
        assert!(!v("zwrt_router.api", "router_set_dmz"));
        assert!(!v("zwrt_router.api", "sim_change_pin"));
        assert!(!affects_network("vendor.call", &json!({})));
        // 表里的每一行都在 VENDOR_CALLS 里（不会因为改名悄悄失效）
        for pair in NETWORK_VENDOR_CALLS {
            assert!(crate::control::VENDOR_CALLS.contains(pair), "{pair:?}");
        }
    }

    #[test]
    fn safety_is_only_switching_off() {
        assert!(is_safety("cellular.set", &json!({"enabled":0})));
        assert!(is_safety("cellular.set", &json!({"roaming":false})));
        assert!(is_safety(
            "cellular.set",
            &json!({"enabled":false,"roaming":0})
        ));
        assert!(!is_safety("cellular.set", &json!({"enabled":1})));
        assert!(!is_safety(
            "cellular.set",
            &json!({"enabled":0,"roaming":1})
        ));
        assert!(!is_safety(
            "cellular.set",
            &json!({"enabled":0,"connect_mode":"1"})
        ));
        assert!(!is_safety("cellular.set", &json!({})));
        assert!(!is_safety("cellular.set", &json!({"enabled":"0"})));
        assert!(!is_safety("network.set_mode", &json!({"enabled":0})));
    }

    #[test]
    fn target_errors_match_the_old_api() {
        let s = find("network.set_mode").unwrap();
        assert_eq!(s.target(&json!({"mode":"Only_LTE"})), Ok("Only_LTE".into()));
        assert_eq!(s.target(&json!({})), Err("missing parameter: mode".into()));
        assert_eq!(
            s.target(&json!({"mode":1})),
            Err("mode must be a string".into())
        );
        assert!(find("band.set_lte").is_none());
    }
}
