# zwrt-datad 性能审计（MU5250，2026-10-04）

对象：本 fork `main@9b89948`，对照上游 `33333s/zwrt-datad` v0.10.56（`54a2395`，合并为 `97f4561`）到 `upstream/main@85786d7`。
本次改动在分支 `perf/ubus-cooldown-fallback`，**不改默认行为、不碰设备**：不设新环境变量时，程序行为和 `main` 完全一样
（后端仍是 `cli`，冷却关着）。设备上试跑见第 7 节。

## 1. 结论先说

1. **上游 v0.10.56 的「原生 ubus + 单一执行者」是从本 fork 学过去的**（上游 `54a2395` 注明出自本 fork #140）。
   本 fork 已有：纯 Rust ubusd 客户端（`rust/src/ubus/client.rs`）、单一执行者（`rust/src/executor.rs`）、块调度与每轮预算（`rust/src/block.rs`）。
2. **最大的瓶颈是部署配置，不是代码**：后端默认 `cli`（`rust/src/ubus/backend.rs` `BackendKind::parse`），
   设备的启动脚本没有设 `ZWRT_DATAD_UBUS`，所以设备上**每个 ubus 调用仍然 fork 一个 `ubus call`**。socket 后端写好了、测过了，但没打开。
3. 上游比我们多的、值得学的只有两点：socket 不可用时**自动退回 CLI**，以及超时对象的**跨轮冷却**。本分支两点都按自己的代码实现了，见第 4 节；
   两点都要靠环境变量打开（`ZWRT_DATAD_UBUS=auto`、`ZWRT_DATAD_UBUS_COOLDOWN_MS=10000`）。
4. 自适应降频、心跳去重、上游的「OK 无数据回 `{}`」、`/state` 加 `ubus_stats`：都**不做**，理由见第 5 节。

## 2. 架构对照

| 项目 | 本 fork（`main`） | 上游 v0.10.56 |
|---|---|---|
| ubus 执行方式 | 单一执行者任务持有后端，同一时间一个请求在途（`executor.rs` `Shared::raw_call`） | 一个 tokio `Mutex<Option<Client>>` 串行（`ubus_socket.rs`） |
| 默认后端 | **`cli`**：每次调用 fork `ubus call`（`backend.rs` `CliBackend::call` → `command::run`） | **socket**；`=cli` 强制 CLI；`=socket` 禁止退回 |
| socket 客户端 | `UbusClient`：HELLO/LOOKUP/INVOKE，seq+peer 过滤，LOOKUP 缓存，断线清缓存，旧连接写失败重发一次（`client.rs` `call`/`call_once`/`request`） | 等价（`Client::request`）；METHOD_NOT_FOUND 不作废 ID |
| 退回 CLI | 无（本分支加 `auto`） | `Failure::NotSent` → 30 秒内走 CLI（`Executor::call`、`mark_socket_down`） |
| 超时处理 | 采集轮 2 秒 / 控制 8 秒（`SocketBackend::set_round`）；超时对象**只在本轮**跳过（`RoundSkips`） | 采集 5 秒 / 交互 8 秒；超时对象冷却 30 秒；查不到的对象 10 秒内不再 LOOKUP |
| 优先级 | 控制任务排队上限 8（`CONTROL_QUEUE`，满了 `Busy`），在块与块之间、`ubus_ttl` 前（`preempt`）、轮间执行；`drain` 按快照计数，防饿死 | 两条道：交互 / 采集；交互排队上限 8；采集调用让位给等待中的交互（`yield_to_interactive`）；无每轮预算、无防饿死 |
| 每轮预算 / 轮转 | 3 秒预算 + 轮转（V2-19、V2-20） | 无 |
| 写闸 | 写超时后探测对象恢复才放下一个写（V2-33，`reopen_gate`，最多 4 次） | 无 |
| 看门狗 | `watchdog.rs`：执行者 30 秒不前进就退出让 procd 拉起（V2-32） | 无 |
| 观测 | stderr 日志；`exec_age_ms` 在心跳和 `/v2/screen` | `ZWRT_DATAD_UBUS_STATS=1` 时 `/state.ubus_stats` |

上游 `54a2395` 之后到 `85786d7` 没有再改 ubus 代码（`git diff 54a2395 upstream/main -- rust/src/ubus_socket.rs` 为空）；
其后改 `state.rs` 的 `26d0309`、`013103e`、`8e0634c` 是电池、MC8531、6 GHz，不涉及 ubus。上游提交和文档里**没有 CPU 数字**。

## 3. 现状清单（`-i 1000`、cli 后端、缓存开）

**每轮的 ubus 调用**（旧采集 `state.rs` `collect`，经 `ubus_ttl`；块 `block.rs` `phase1_blocks`）：

| 间隔 | 调用 | 次/秒 |
|---|---|---|
| 每轮 | `system info`、`zte_nwinfo_api nwinfo_get_netinfo`、`zwrt_data get_wwandst {type:1}` | 3 |
| 5 秒 | `zwrt_router.api router_get_status_no_auth`、`zwrt_bsp.thermal get_cpu_temp`、`network.interface.{lan,zte_wan,zte_wan6} status`、`zwrt_data get_wwaniface`；块 `zwrt_bsp.battery list`、`zwrt_bsp.charger list` | 1.6 |
| 10 秒 | `get_wwandst {type:4}`、两个接入列表、两次 `zte_libwms_get_sms_data` | 0.5 |
| 30 秒及以上 | SIM、USB、短信容量、通用信息、流量限额/清零日、NFC、LAN 信息、`system board`、IMEI | 约 0.2 |

合计约 **5 次/秒**，每次一个 `ubus call` 进程。轮内没有重复调用（`get_wwandst` 两次参数不同）。
重复只出现在控制/事务路径（`ops/mod.rs`、`sms.rs`、`control.rs` 各自读 SIM、netinfo 等），频率低，不影响稳态。

**其他 fork**：
- `extra_wifi::tick`（`extra_wifi.rs`），每 5 秒一次：两个 `datad_wifi.datad_ssid_{1,2}` 段各一次 `uci get`（`state::uci_read`），没配置时约 0.4 次/秒。
- UCI 包正常在进程内解析（`uci.rs`），`/tmp/.uci` 有未提交改动时才 fork `uci show`。
- 常驻子进程：`ubus listen`（短信事件，`ubus/listen.rs`）。

**推送**：
- `/events`：用 `watch`，整份快照比较（不比 `ts`），变了才发；每个客户端各自序列化一次 JSON（`server.rs` `refresh_snapshot`）。
- `/v2/events`：每个事件只序列化一次，用 `Bytes` 共享（`v2.rs` `frame`），发布前按块策略比较有无变化（`block.rs` `Hub::publish`）。
- 心跳：每轮一条，另有独立 5 秒定时器。

**锁与阻塞**：
- tokio 多线程运行时（`main.rs` `#[tokio::main]`）。
- 执行者用 `std::sync::Mutex`，只做短临界区。
- `/proc`、sysfs 和 UCI 文件读取是同步 `std::fs`，都在执行者任务里，单次很小。
- `qos.rs` `read_tail` 每 30 秒同步读最多 2 MiB×2 的日志尾（`MAX_LOG_BYTES`），是唯一一处较大的同步读，记为低优先级。

**控制请求不会被采集饿死**：
- 控制任务在每块之前、旧采集每个 `ubus_ttl` 之前（`preempt`）、轮间执行；`drain` 只做进入时已排队的任务，持续进来的控制请求也挡不住采集轮和心跳。
- 已有测试证明这几点：`control_runs_before_next_block`、`legacy_calls_are_preempted_by_control_and_not_budget_cut`、
  `control_queue_ten_requests_eight_ordered_two_busy`、`rounds_and_heartbeats_continue_under_control_flood`（`rust/src/executor/tests.rs`）。
- 最坏等待是一个在途调用：cli 8 秒；socket 采集轮里 2 秒。

**消费方依赖**（决定哪些「优化」不能做）：
- 触屏（touch-ui `src/data.c`）只订 `/events`：
  - 任何字节都算活着，45 秒静默就重连；靠 axum 默认 15 秒 keep-alive（`server.rs` `KeepAlive::new()`）。
  - 亮屏、息屏分别设 1000 / 5000 ms 采样间隔（`state.set_interval`）。
  - `/v2/screen` 按快照版本拉，`exec_age_ms` ≥ 20 秒算执行者卡住。
- zte-agent（manager `zte-agent/src/datad_feed.rs`）订 `/v2/events`：
  - 20 秒没**事件**就判静默，keep-alive 注释不算，所以心跳必须真发。
  - `seq` 必须连续。
  - 电池/充电块 stale 超过 30 秒就自己读 ubus，并写降级标记。

**消费方读、但文档（`docs/*.md`、`rust/COMPATIBILITY.md`）没写明的字段**（本分支一个都没动）：
- `/state`：
  - `net.nr_band`
  - `interfaces.cellular.{enable, connect_status, roam_enable}`（schema 里只写了 `"cellular": {}`）
  - `net.wan_dns`；`uci_device_info.wan_dns` 是 shell 引号包着的字符串
  - 旧 C 版名字 `net.cell_id` / `net.channel`，消费方用来兜底 `lte_cell_id` / `lte_channel`
- `/v2/screen`：`net.mode_auto`、载波的 `sinr_tone`。
- 缺字段语义：
  - 触屏多数字段「缺 = 0」；例外：`cpu_usage` 缺 = -1，`wlan.enabled` 缺 = 1，`cellular.enable`/`roam_enable` 缺 = -1（不知道），`exec_age_ms` 缺 = -1（旧 datad）。
  - agent：块缺 `stale` 算 `true`，`data: null` 算「不知道」。
  - 这些都应补进文档，单独做。

## 4. 瓶颈排序与本分支的改动

| # | 瓶颈 | 影响 | 处理 |
|---|---|---|---|
| 1 | 设备跑 `cli` 后端，约 5 次/秒 fork `ubus call` | 设备实测 `ubus call` 3～5 ms/次，socket 0～1 ms/次（9-29 只读探测）；估算省约 1.5～2.5% 单核，**是估算，未在设备上量** | 本分支加 `auto`（socket + 肯定没送到才退回 CLI），设备按第 7 节旁路试跑后再把启动脚本换成 `auto` |
| 2 | 旧采集里挂住的对象每轮都吃满超时（cli 8 秒、socket 2 秒） | 每轮都被拖慢，别的数据也跟着变旧 | 本分支加跨轮冷却（`ZWRT_DATAD_UBUS_COOLDOWN_MS`，默认关，建议 10000） |
| 3 | `extra_wifi::tick` 每 5 秒 2 次 `uci get` | 约 0.4 次/秒 fork | 待办：改用进程内 UCI 解析，先核对 `uci -q get` 在缺选项、`/tmp/.uci` 有改动时的行为 |
| 4 | qos 每 30 秒同步读 ≤4 MiB | 偶发几毫秒阻塞执行者 | 待办，低优先级 |
| 5 | `/events` 每个客户端各自序列化一次 | 只有触屏一个客户端，等于一次 | 不做 |

**改动 A：跨轮冷却**（`executor.rs` `Shared::raw_call`、`cooling`；`Config::cooldown`；`server.rs` 读环境变量）
- 只管旧采集里的调用：超时后，该对象在冷却期内的采集调用直接返回 `Skipped`，不发请求。
- 块不冷却：块有 V2-21 的 5 秒失败重读，电池/充电不能空到 agent 的 30 秒 stale 线。
- 控制任务、内部任务、写闸探测都不管冷却。
- **默认关**：不设变量时和原来一样只按本轮跳过，免得以后哪次顺手换 datad 时，把没试过的行为带上设备。
- 建议值 10 秒，不照搬上游的 30 秒：我们 socket 采集超时只有 2 秒，`zte_nwinfo_api` 等基带对象在 socket 上还没核实过，
  一次「慢但有效」的回复就可能踩到超时，30 秒会让信号数据空太久。`0` 关掉，恢复原来「只本轮跳过」。
- 已有的 `ubus_ttl` 对 TTL>0 的调用本来就把失败缓存 5 秒，所以冷却主要作用在每轮都读的三个对象上。

**改动 B：`ZWRT_DATAD_UBUS=auto`**（`backend.rs` `AutoBackend`；`client.rs` `last_call_not_sent`）
- 客户端记录本次 INVOKE 帧是否「可能已送到」：写成功，或写到一半超时，都算可能已送到；写出错不算。
- 后端只在**传输错误**（`Io`/`Timeout`/`Protocol`）**并且**请求肯定没送到时，这一次改用 `ubus call`，
  之后 30 秒（`FALLBACK_RETRY`）都走 CLI，到时再试 socket。
- 服务报错、找不到对象、OK 无数据都是 ubusd 的回答，不退回。
- INVOKE 写出去之后的超时不退回、不重发：写操作不能做两次（测试 `auto_never_resends_an_invoke_that_may_have_run`）。
- 时间上限：退回的那一次 CLI 只拿 8 秒减去 socket 已用掉的时间（至少 0.5 秒）。
  所以 ubusd 收连接却不回 HELLO 时，一次调用也不会变成「8 秒 socket + 8 秒 CLI」；
  执行者的 `CALL_LIMIT`（10 秒）、看门狗，以及触屏和 agent 的 20 秒卡住线，前提都不变（测试 `auto_fallback_stays_within_the_cli_timeout`）。
- `socket` 保持「从不退回」，默认仍是 `cli`：设备上的账本按启动环境记 `ubus=…`，默认值不能在程序里悄悄变。

**本分支没改的**：`/state`、`/events`、`/v2` 的字段和推送节奏；`/control` 语义；块表；golden 全部不变（CI 第 12、13 步）。

## 5. 不做的事和理由

- **没有订阅者时降频**：触屏和 agent 一直连着，「没人订阅」实际不出现；触屏已经按亮屏/息屏切 1 秒/5 秒。
- **心跳去重 / 降低心跳**：agent 20 秒没事件就判静默，而且心跳占 `seq`。
- **上游「OK 无数据回 `{}`」**：会改变读取的缺字段语义；写路径已在 `1aad006`、`648bc5f` 把空回复算成功。
- **`/state` 加 `ubus_stats`**：会破坏 golden；新字段只进 `/v2`。要计数先用 stderr。
- **负 LOOKUP 缓存（上游 10 秒）**：socket 上 LOOKUP 0～1 ms，而且旧采集的失败已经缓存 5 秒，收益太小。
- **按调用粒度让位给交互请求（上游 `yield_to_interactive`）**：我们的安全点更粗（`state::ubus` 不是安全点，因为 `sms::prepare` 持锁调 ubus），
  但最坏等待是「一个在途调用」，socket 下 ≤2 秒。换成 socket 后再看有没有必要。

## 6. 基准

**可在开发机/容器跑的**（`rust/src/ubus/tests.rs` `bench_socket_vs_cli`，`#[ignore]`，不进 CI）：

```
cargo test --release bench_socket_vs_cli -- --ignored --nocapture   # BENCH_N 默认 500
```

2026-10-04 在 Docker（rust:1.89.0，Apple Silicon 宿主）上的结果，同一个只读调用 500 次：

| 后端 | p50 | p95 | p99 | 最大 | 起进程 |
|---|---|---|---|---|---|
| socket（mock ubusd，1 条连接） | 4 µs | 5 µs | 6 µs | 121 µs | 0 |
| cli（`tests/mock_ubus.sh`，sh 脚本） | 0.56 ms | 0.99 ms | 1.40 ms | 2.73 ms | 500 |

这只说明相对量级。mock CLI 是 shell 脚本，不是设备上的 `ubus` 程序，宿主 CPU 也远快于设备，**不能当设备数字用**。

**设备上已有的基线**（之前只读测得）：
- 9-25：datad 约 4.3% 单核（10 秒 43 tick）；整机 fork 17.6 次/秒（清闲），平时 21～31 次/秒。
- 9-29：socket 每次 0～1 ms，`ubus call` 每次 3～5 ms。三个只读调用、41 个值，两种后端完全一致。

**确定性的行为测试**（tokio 可控时钟，进 CI）：
- `legacy_timeout_cools_object_across_rounds_then_expires`
- `cooldown_ignores_blocks_failures_and_can_be_off`
- `cooled_object_keeps_heartbeats_flowing`：对象一直挂着时，60 秒里调用不超过 4 次（不冷却约 7 次），心跳间隔 ≤10 秒
- `client_reports_whether_the_invoke_may_have_been_sent`
- `auto_falls_back_to_cli_while_ubusd_is_down_then_retries_socket`
- `auto_never_resends_an_invoke_that_may_have_run`
- `auto_fallback_stays_within_the_cli_timeout`
- `cooldown_env_parsing`

**本地没量的**：控制请求排队等待时间、`/state` 新鲜度、SSE 推送延迟、RSS 和峰值内存、长时间运行。
这些只有在设备上量才有意义，放在第 7 节。

## 6b. 设备实测（2026-10-04，息屏、采样间隔 5 秒）

1. **只读核对**（`--ubus-compare`，程序放 `/tmp` 跑一次后删掉）：
   - 11 个对象、179 个值，socket 和 `ubus call` 完全一致。
   - 对象包括 `zte_nwinfo_api nwinfo_get_netinfo`、`zwrt_zte_mdm.api get_sim_info`、`zwrt_router.api`、`zwrt_bsp.{thermal,charger,battery,usb}`、`network.interface.{lan,zte_wan}`、`zwrt_wms` 容量。
   - 对象 ID 和 `ubus -v list` 一致；1 次连接，丢帧 0，超时 0。
   - 前后 ubusd、datad 进程号和 boot id 都不变。
   - 单次耗时：socket 0～5 ms，`ubus call` 3～6 ms；`zwrt_wms` 两边都约 15 ms，时间花在原厂服务本身。
2. **基线**（现行 datad，cli 后端，600 秒）和**候选**（本分支，`ZWRT_DATAD_UBUS=auto`，冷却关；旁路试跑预热 60 秒后量 420 秒）：

| 指标 | 基线 cli | 候选 auto | 备注 |
|---|---|---|---|
| datad CPU（单核） | 2.73% | **1.20%** | 约 -56% |
| 整机 fork 次数/秒 | 94.1 | 77.9 | 整机计数，含其他程序的波动，只作参考 |
| VmRSS / VmHWM | 28.8 / 32.8 MB | 15.4 / 19.2 MB | 进程年龄不同（候选刚起），不能直接比 |
| `/v2/screen` p50 / p95 / 最大 | 3.6 / 4.5 / 4.5 ms | 3.3 / 4.1 / 4.7 ms | 各 30 次 |
| 30 秒内心跳 | 6 | 6 | 息屏间隔 5 秒 |
| 退回 CLI | — | 0 次 | |

- 旁路试跑守护 1200 秒内无异常，结束后自动恢复正式版。
- 局限：
  - 两段测量不是同一时间，窗口长度不同（600 秒 / 420 秒）。
  - 只量了息屏；亮屏 1 秒一轮时调用次数约多 5 倍，预计收益更大，但没量。
  - 没有量控制请求往返。

## 7. 设备试跑方案（2026-10-04 已按第 1～3 步做过一次，结果见 6b）

每一步都要用户同意；先拿设备锁，按 datad 旁路试跑流程。

1. **只读核对基带对象**：`zwrt-datad --ubus-compare zte_nwinfo_api:nwinfo_get_netinfo zwrt_data:get_wwaniface zwrt_zte_mdm.api:get_sim_info …`。9-29 只核过 `system`、`zwrt_bsp.battery`。
2. **基线**：在现行 datad（cli）下用同一套采样脚本记：
   - datad 的 `/proc/<pid>/stat` utime+stime 增量
   - 整机 fork 次数（`/proc/stat` processes 增量）
   - RSS / VmHWM
   - 心跳间隔、`exec_age_ms` 最大值
   - `/v2/screen` 拉取延迟 p50/p95/p99
   - 一次 `state.set_interval` 控制往返延迟
3. **候选**：旁路起本分支的程序，带 `ZWRT_DATAD_UBUS=auto ZWRT_DATAD_UBUS_COOLDOWN_MS=10000`。同样负载、同样亮屏状态，采同样的指标，另看 stderr 里有没有退回、冷却的日志。
4. **故障**：试跑期间不重启 ubusd、不断 WAN（会动原厂服务）；这两种情况只用 mock 测试覆盖，真机只在自然发生时看日志。
5. **时长由用户定**：新传输方式要承载 `zte_nwinfo_api`、`zwrt_zte_mdm.api` 这些基带读取，按「碰基带的长观察」规则，
   旁路 10 分钟只够查崩溃和明显异常；正式切换前观察多久，由用户决定。
6. **判定**：
   - datad CPU 和整机 fork 明显下降，心跳间隔不变差，0 次退回，golden 字段一致，才把启动脚本改成 `auto`（单独提交，并更新账本检查项）。
   - 否则保持 `cli`。

## 8. 后续计划

| 项 | 复杂度 | 风险 |
|---|---|---|
| 设备按第 7 节试跑，通过后启动脚本加 `ZWRT_DATAD_UBUS=auto`（冷却另定） | S | 基带对象在 socket 上没核实过：先做第 1 步 |
| 把第 3 节列出的未文档化字段和缺字段语义补进 `docs/STATE_SCHEMA.md` | S | — |
| `extra_wifi` 的 `uci get` 改进程内解析 | S | `/tmp/.uci` 改动和缺选项的语义要先对齐 |
| qos 日志尾读改增量读（记住偏移） | S | 日志轮转 |
| 短信事件改直连 ubusd 订阅（去掉常驻 `ubus listen`） | M | 事件路径在设备上没核实过（STATE_V2 第 241 行附近） |
| 冷却时长按试跑数据复核（10 秒是暂定） | S | — |
