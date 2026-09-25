//! ubus 客户端（docs/STATE_V2.md V2-18、V2-19、V2-21）：纯 Rust 直连 ubusd，替代每次起 `ubus call`。
//!
//! - `blob`：blob/blobmsg TLV 编解码、ubus 帧头、协议常量（出处和待核实项见该文件顶部）。
//! - `client`：`UbusClient`（HELLO/LOOKUP/INVOKE/DATA/STATUS，seq+peer 过滤，超时重连，LOOKUP 缓存与失效），
//!   `UbusError`、`RoundSkips`（本轮跳过超时对象）。
//! - `backend`：`ZWRT_DATAD_UBUS=cli|socket` 选后端，`UbusBackend` trait、`Backend`。
//!
//! T4 接入：采集执行者还没用它们，所以整个模块暂时允许未使用。
#![allow(dead_code)]

pub mod backend;
pub mod blob;
pub mod client;

#[cfg(test)]
#[path = "../../tests/support/mock_ubusd.rs"]
mod mock_ubusd;
#[cfg(test)]
mod tests;

/// 对象名、方法名的校验，和 `state::ubus` 的 `validate_name` 一致（错误文字也一致）。
pub(crate) fn validate_name(v: &str) -> Result<(), String> {
    if v.is_empty()
        || v.len() > 128
        || !v
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err("invalid ubus name".into());
    }
    Ok(())
}
