//! 动作描述表（write-op-layer.md「动作描述表 ActionSpec」）：哪些 `/control` 动作走事务、改的是哪一项、
//! 目标值从哪个参数来、确认时限多长。T2 先只接网络模式；其余动作照旧走 `control::execute`。
//! 读回和写怎么做在 `UbusDevice`（按 `item` 分）。

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
}

pub const NETWORK_MODE: &str = "network.mode";

pub static SPECS: &[Spec] = &[Spec {
    action: "network.set_mode",
    item: NETWORK_MODE,
    param: "mode",
    deadline_ms: 120_000,
    deadline_env: "ZWRT_DATAD_DEADLINE_NETWORK_MODE_MS",
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
