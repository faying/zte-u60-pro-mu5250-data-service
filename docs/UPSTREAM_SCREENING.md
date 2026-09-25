# 上游提交筛选清单（阶段 0）

- 筛选日期：2026-09-25
- 上游：`33333s/zwrt-datad`，`upstream/main` = `4d30001`（Merge PR #79）
- 本 fork：`main` = `179f3d6`（按已提交内容筛，用 `git show main:路径` 看，不看工作区）
- 分叉点：`b7e3043`（上游 PR #63，Rust v0.10.0）

## 规则摘要

1. 本 fork 不再跟上游，datad 要改成设备上唯一的数据层。这份清单一次筛完所有上游提交，之后不再逐个跟。
2. **旧接口 `/state`、`/events`、`/control` 的输出要逐字节不变。** 会改旧输出的提交不能原样挑，只有两种做法：
   - 改写成旧输出不变，新字段或新形态只放进 `/v2`（见 `docs/STATE_V2.md`）；
   - 或者不挑。
3. 已删掉的模块：C 版和 Go、云端（cloud/NMS）、OTA、WebShell、`/ubus` `/ubus/list` `/ubus/call` 透传、上游安装器和发布脚本、外部更新源。涉及这些的提交一律标「不挑（已删）」。
4. 完全自主、不依赖外部：加回外部更新源、云端、遥测、远程登记的提交都不挑。
5. 机型是 MU5250（U60 Pro），固件 B27。只和别的机型（主要是 MU5252 / TopFlow）有关的提交一般不挑。旧代码里 `aggregation` 和 `cooling` 两块只在 `template == "MU5252"` 时输出（`rust/src/state.rs:1426` 那个分支），MU5250 上不存在。

## 数量

- `git log main..upstream/main`：**46 个提交**，其中 **15 个是合并提交**，**31 个是普通提交**。
- `git cherry main upstream/main`：31 个全是 `+`，**没有已经等价挑过的提交**。
- 虽然补丁不等价，以下内容本 fork 已经自己做过，功能上有重叠：
  - 删掉 C/Go 实现：`22613e1`、`b7796ec`（对应 `b8828c1` 的主体）。
  - qtrace 掩码：`b8828c1` 把掩码搬进 `rust/src/qtrace_mask.rs`；本 fork 是把 `qtrace_mask.h` 放进 `rust/src/` 后 `include_str!`，效果相同。
  - 慢数据缓存和 qos 扫描节流：`b5e8786`（和 `5d859e5`、`5b908db` 的缓存部分有重叠）。
- 合并提交只把下面的普通提交带进来，不单独决定：`eb753be`(#64)、`4e12f7e`(#66)、`ac00942`(#67)、`969f8b7`(#68)、`779319f`(#69)、`5993449`(#70)、`080f53b`(#71)、`0175d55`(#72)、`0071395`(#73)、`c044b14`(#74)、`6da84b3`(#75)、`7561626`(#76)、`716c3c4`(#77)、`0268ce2`(#78)、`4d30001`(#79)。
- 一个提交如果要拆开挑，按其中**最能挑的那部分**计数，并在表里注明拆法。
- 31 个普通提交的结论：**挑 1 个，改写后挑 8 个，不挑 22 个**。

## 逐个筛选

「旧输出」一列：是 = 会改 `/state`、`/events`、`/control` 的输出；否 = 不改；部分 = 只有一部分改。

优先级：P1 = 修的是影响 MU5250 的 bug，先做；P2 = 有用，但要改写或要先核对；P3 = 可选；— = 不挑。

| 提交 | 标题 | 改动区域 | 旧输出 | 和已删模块的关系 | 建议 | 优先级 |
|---|---|---|---|---|---|---|
| `b8828c1` | release: clean main to Rust-only v0.10.1 | 删 C/Go/cloud/include/src；CI；安装器；Rust 小修：`wifi.rs` `match_live`、`control.rs` `boolean()`、`state.rs` `ubus()` 空回复、`sms.rs` 写法、`qtrace_mask.rs` | 部分：`boolean()` 放宽为接受 0/1，失败时的错误文本从 `must be boolean` 变成 `must be boolean or 0/1`（`/control` 错误回复） | 主体（删 C/Go、cloud、安装器）已由 `22613e1`/`b7796ec` 做完 | **改写后挑（拆开）**：`match_live` 修复原样挑。本 fork 现在的 `(matches.len()==1).then_some(matches[0])` 会立即求值，没有匹配的 AP 时越界 panic；release 用的是 `panic = "abort"`，整个 datad 进程会退出。目前只有 `wifi.advanced.status` 查询和 `wifi_dbm` 控制会走到这里，现有的触屏和 zte-agent 都不调用。`boolean()` 如果要挑，保留旧错误文本。`ubus()` 空回复当 `{}`：只在还走 ubus 命令行时有意义，纯 Rust ubus 客户端接上后在客户端层处理。其余不挑 | P1（`match_live`）；P3（其余） |
| `fc020a2` | fix: report TopFlow ICG v3 telemetry | `state.rs`：`icg-client` 聚合遥测、每轮扫 `/proc` | 否（只在 MU5252 分支里、并且 `icg-client` 在跑时才改 `aggregation`） | 无 | **不挑**：只和 MU5252/TopFlow 有关，另外每轮全扫 `/proc` 会增加开销 | — |
| `8c0a33b` | fix: report automatic liquid cooling state | `state.rs` `cooling_state` | 是（`cooling.liquid` 新增字段），但只在 MU5252 上输出 | 无 | **不挑**：只和 MU5252 有关 | — |
| `d2e0655` | fix: remove OTA rollback after successful install | `scripts/install.sh.in`、安装器测试 | 否 | 上游安装器、OTA（已删） | **不挑（已删）** | — |
| `824c75c` | fix: expose cellular lock readback fields | `state.rs` `net` 块新增 `lte_band_lock`、`gw_band_lock`、`nr5g_*_band_lock`、`lock_lte_cell`、`lock_nr_cell`、`lte_action_channel` | 是（所有机型的 `net` 都多出 8 个字段） | 无 | **改写后挑（只进 /v2）**：锁频和锁小区的读回对 MU5250 有用，但只进 `/v2` | P2 |
| `75e67c6` | fix: verify partial cell unlock results | `control.rs` `cell.unlock_all` | 是（成功回复从原厂返回值变成 `{"verified","lte_call_ok","nr_call_ok"}`；LTE 失败后仍会尝试 NR 并读回核对） | 无 | **改写后挑**：「LTE 失败也继续解 NR、读回核对」的行为值得要。旧 `/control` 的回复形态保持不变，新形态只放 `/v2` 控制队列。部分失败时的语义要定（见文末） | P2 |
| `250f5c0` | fix: use active TopFlow traffic subid | `state.rs`：MU5252 按 `current_sim_slot` 加 `subid`、改用 `zte_mwan2`；`traffic` 新增 `month_time`；`get_sim_info` 提前调用 | 部分：MU5250 上只多出 `traffic.month_time` | 无 | **不挑**：主体只和 MU5252 有关。`month_time` 要的话在 `/v2` 单独加，不挑这个提交 | — |
| `5d859e5` | fix: cache complete qos logs | `qos.rs` 改成增量读、完整扫描 `key.log` 两份（原来只读尾部 2 MiB）；`control.rs`/`server.rs` 加 `qos::invalidate()`；`state.rs` MU5252 部分复用；`STATE_SCHEMA.md` | 部分：字段不变，但 `qos.qci/ambr_*` 的**值**可能变（能扫到尾部 2 MiB 以外的记录） | 无 | **改写后挑**：增量读省 CPU，对 MU5250 有用。但它和我们 `b5e8786` 的 30 秒节流重叠；`state.rs` 那部分依赖 `250f5c0` 的 `active_subid`（只取 `qos.rs`）；`data_lines` 会一直涨，没有上限。旧 `/state` 的值要保持和现在一样（例如继续只看尾部窗口），完整扫描的结果只给 `/v2`，或者由用户决定 | P2 |
| `ab131c5` | fix: separate band capabilities from current locks | `state.rs`：`*_supported_bands` 改读 `zwrt_zte_nwinfo.default_band_lock`，新增 `net.band_capabilities`；mock、测试、文档 | 是（`net.*_supported_bands` 的值变了，还多出 `net.band_capabilities`） | 无 | **改写后挑（只进 /v2）**：修的是真 bug，旧代码把「当前锁频」当成「设备支持的频段」输出。触屏 `ui.c` 里显示的「可用频段」就是这个错值。旧字段按规则保持原值不变，正确的能力目录只在 `/v2` 输出，触屏迁到 `/v2` 后才能显示对 | P1 |
| `df8a62d` | feat: expose authenticated webshell on LAN | `server.rs`、文档、webshell 测试 | 否 | WebShell（已删） | **不挑（已删）** | — |
| `0a34f11` | feat: accept signed datad update requests from NMS | `cloud.rs`、`cloud_update.rs` | 否 | 云端 + OTA（已删）；属于外部更新源 | **不挑（已删）** | — |
| `25db942` | fix: keep NMS update jobs alive across MQTT reconnects | `cloud.rs`、`cloud_update.rs` | 否 | 云端 + OTA（已删） | **不挑（已删）** | — |
| `884dbf4` | fix: keep cloud presence alive between telemetry reports | `cloud.rs` | 否 | 云端、遥测（已删） | **不挑（已删）** | — |
| `8cd41fb` | fix: release expired remote bridge session slots | `cloud.rs` | 否 | 云端（已删） | **不挑（已删）** | — |
| `3309fa7` | fix: consume cloud configuration changes before reconnecting | `cloud.rs` | 否 | 云端（已删） | **不挑（已删）** | — |
| `8bd953d` | Add verified MQTT over WSS transport for cloud connections | `cloud.rs`、`Cargo.toml/lock`、云端文档 | 否 | 云端（已删），还会加回依赖 | **不挑（已删）** | — |
| `4904edb` | feat: add opt-in native NMS WebShell over reverse WSS | `cloud.rs`、`cloud_shell.rs`、`webshell.rs`、`server.rs` | 否 | 云端 + WebShell（已删） | **不挑（已删）** | — |
| `8e3d116` | fix: bound native WebSocket allocations before record decoding | `cloud_shell.rs` | 否 | 云端 WebShell（已删） | **不挑（已删）** | — |
| `bb37909` | test: verify idle native terminal answers keepalive ping | `cloud_shell.rs` 测试 | 否 | 云端 WebShell（已删） | **不挑（已删）** | — |
| `95bc208` | feat: allow explicitly configured alternate NMS remote origins | `cloud.rs`、云端文档 | 否 | 云端（已删），属于外部源 | **不挑（已删）** | — |
| `9139735` | fix: restore reliable fan curves and thermal fallback | `cooling.rs`、`state.rs` cooling 块、`server.rs` 退出时 `cooling::shutdown()`、`scripts/service.sh` 查找 thermal 路径、测试、CI | 是（`cooling.fan` 新增 `policy`/`temperature_source`），只在 MU5252 上输出 | 无 | **不挑**：只和 MU5252 风扇有关 | — |
| `bebbe19` | test: await asynchronous PTY cleanup and report control failures | `cloud_shell.rs` 测试；`tests/rust_control_integration.sh` 控制失败时打印 HTTP 码和回复 | 否 | `cloud_shell.rs` 那部分属于云端（已删） | **挑（只取 `tests/rust_control_integration.sh`）**：只改测试，让控制失败时报得清楚 | P3 |
| `463c1a0` | fix: preserve Wi-Fi settings and use native band steering | `control.rs`、`server.rs`、`wifi.rs`：双频合一改读写 `zwrt_wlan` 的 `zte_mbb.lbd`，不再借用 `router_set_wifi_isolate`；`wifi.configure` 检查 reload 结果并读回；`wifi.status` 多返回 `hidden/isolate/pmf/maxassoc` | 是：`wifi.status` 增加字段；`wifi.dual_band_status` 增加 `supported/enabled`；`wifi.configure` 增加 `verified`；`wifi.set_dual_band` 回复形态也变了 | 文档里提到的「通用 ubus 透传」本 fork 已删 | **改写后挑**：真正要修的是现在的 `wifi.set_dual_band` 会读改写 `router_set_wifi_isolate`（网络隔离那组设置），可能碰到隔离设置。旧回复形态保持不变，新字段只进 `/v2`。挑之前要先只读核对 B27 上 `zte_mbb.lbd` 存在、语义一致 | P2（核对后可升 P1） |
| `5b908db` | fix: restore encrypted SMS reads and bounded list synchronization | `sms.rs`：兼容单行 PEM 公钥、检查原厂 RSA 注册结果、解密失败重建一次会话、读写串行、分页同步（最多 512 条）、缓存；`control.rs` 删除/标已读后失效缓存；`state.rs` 改用 `sms::snapshot()` | 是：`sms` 新增 `stale`、`truncated`、`error`；`sms.list` 从每个存储最多 8 条变成最多 256 条 | 无 | **改写后挑**：会话恢复和 PEM 兼容是 MU5250 短信读取的稳定性修复，旧输出不变的情况下可以挑。`stale/truncated/error` 和更长的列表只进 `/v2`；旧 `sms.list` 仍然只给每个存储最新 8 条。删除和标已读后失效缓存和我们 `b5e8786`「/control 前后清缓存」重叠 | P1（会话恢复部分） |
| `110d71e` | fix: await cooling driver writes before readback | `cooling.rs` | 否（只在 MU5252 上生效） | 无 | **不挑**：只和 MU5252 有关 | — |
| `87cf8ea` | feat: expose read-only USB negotiated link speed | 新文件 `usb.rs`；`/state` 新增顶层 `usb`；新路由 `/usb/status`；`usb.set` 回复加 `link`；`--usb-status` 命令行 | 是（`/state` 多一个顶层键，`/control usb.set` 回复多出 `link`） | 无 | **改写后挑（只进 /v2，可选）**：只读 sysfs，不依赖外部。要不要这个功能由用户决定 | P3 |
| `8e290b1` | fix: keep cooling always-on modes constant and synchronized | `cooling.rs`、`state.rs` cooling 块（加 `vendor_sync_error`，每轮同步原厂开关） | 是，只在 MU5252 上 | 无 | **不挑**：只和 MU5252 有关 | — |
| `aaa3fd2` | fix: restore liquid thermal control if manual drive fails | `cooling.rs` | 否（只在 MU5252 上生效） | 无 | **不挑**：只和 MU5252 有关 | — |
| `37a11f8` | feat: add persistent Keymaster identity and challenge proof APIs | 新 `identity.rs`、`keymaster_worker.rs`（动态链接原厂 Keymaster）、`/identity/*` 三个接口、`Cargo.toml` 加 `ring`、`build.rs`、`scripts/build.sh` | 否（新接口，不改旧接口） | 用途是向运营方做账户、许可证、付费登记（上游文档原话），属于云端或远程登记 | **不挑**：违反「完全自主、不依赖外部」；还要动态加载原厂库、增加一个可执行文件 | — |
| `e8cfab4` | fix: fail closed on lost identity material and restrict worker dumps | `identity.rs`、`keymaster_worker.rs` | 否 | 同上 | **不挑**：依赖 `37a11f8` | — |
| `b3b5647` | test: stabilize identity rate-window and initialization failure coverage | `identity.rs`、测试 | 否 | 同上 | **不挑**：依赖 `37a11f8` | — |

## 建议先挑的

按顺序：

1. **`b8828c1` 里的 `match_live` 修复**：原样挑，不改输出。修的是会让整个进程 abort 的越界。
2. **`5b908db` 的会话恢复部分**：PEM 兼容、注册结果检查、解密失败重建会话、读写串行。旧 `sms` 输出保持不变，`stale/truncated/error` 和完整列表进 `/v2`。
3. **`ab131c5` → `/v2` 的 `band_capabilities`**：旧字段不动。触屏的「可用频段」迁到 `/v2` 后才会显示对。
4. **`463c1a0`**：双频合一不再借用网络隔离接口。先只读核对设备，再做改写版，旧回复形态不变。
5. **`824c75c` → `/v2`**：锁频和锁小区的读回字段。
6. **`bebbe19` 的测试脚本部分**：控制失败时报清楚。

## 需要用户决定的

1. **`5d859e5` qos 完整扫描**：要不要从「只看 `key.log` 尾部 2 MiB + 30 秒节流」（`b5e8786`）改成「增量读、完整扫描」？
   - 好处：CPU 更省，值更全。
   - 代价：缓存会随日志涨，要自己加上限；如果旧 `/state` 的值也跟着变，就违反逐字节不变。建议只在 `/v2` 用新结果。
2. **`463c1a0` 核对设备**：要确认 B27 上 `zwrt_wlan.wlan_uci_get_section` 能读到 `zte_mbb.lbd`，并且语义就是双频合一。这是一次只读 ssh 检查，但要用户点头再做（另一个会话可能正在动设备）。
3. **`75e67c6` 部分失败时怎么算**：LTE 或 NR 某一边解锁调用失败、但读回显示已经解开，算成功还是失败？旧接口维持旧行为（LTE 失败就直接失败），`/v2` 用哪种要定。
4. **`87cf8ea` USB 协商速率**：要不要这个功能（只读，放进 `/v2`）？
5. **`250f5c0` 的 `traffic.month_time`**：`/v2` 要不要这个字段（不挑原提交，单独加）？
6. **`b8828c1` 的 `boolean()` 接受 0/1**：放宽 `/control` 的输入要不要做？做的话旧错误文本保持不变。
7. **`b8828c1` 的 ubus 空回复当 `{}`**：要和纯 Rust ubus 客户端（T3）的工作对齐。客户端接上前，命令行路径是否先补这一行？
