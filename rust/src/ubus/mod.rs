//! ubus 客户端（docs/STATE_V2.md V2-18、V2-19、V2-21）：纯 Rust 直连 ubusd，替代每次起 `ubus call`。
//!
//! - `blob`：blob/blobmsg TLV 编解码、ubus 帧头、协议常量（出处和待核实项见该文件顶部）。
//! - `client`：`UbusClient`（HELLO/LOOKUP/INVOKE/DATA/STATUS，seq+peer 过滤，超时重连，LOOKUP 缓存与失效），
//!   `UbusError`、`RoundSkips`（本轮跳过超时对象）。
//! - `backend`：`ZWRT_DATAD_UBUS=cli|socket` 选后端，`UbusBackend` trait、`Backend`。
//!
//! 执行者（`executor.rs`，T4）持有一个 `Backend`，datad 的全部 ubus 调用都经过它。
//! 客户端里有些接口只给测试和日志用（统计、缓存查询），所以整个模块允许未使用。
#![allow(dead_code)]

pub mod backend;
pub mod blob;
pub mod client;

#[cfg(test)]
#[path = "../../tests/support/mock_ubusd.rs"]
mod mock_ubusd;
#[cfg(test)]
mod tests;

/// 对象名、方法名（以及 uci 包名）的校验；错误文字和原来 `state::ubus` 的一致。
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
