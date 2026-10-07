//! 投影层：纯函数，给什么输入出什么结果，不发请求、不读文件、不看时钟（Phase 2a，D12）。
//! 层次划分见 `rust/LAYERS.md`。
pub mod snapshot;
