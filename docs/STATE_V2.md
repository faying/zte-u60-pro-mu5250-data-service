# zwrt-datad `/v2` 状态流

这份文档规定 `/v2/events`、`/v2/state` 的格式，以及 datad 内部采集和控制排队的规则。
阶段 1 编码（ubus 客户端、单一采集执行者、`/v2/events`）以这里为准。

- 每条规则有编号 **V2-N**，后面跟着验证它的测试名；「测试（Tn）」表示这个测试在哪个实施任务里写。
  `tests/state_v2_doc_check.sh` 会检查每条规则都有测试名、测试名不重复；
  加 `--impl` 时还会检查 data-service 这边的测试函数都已经写出来了（T3–T5 做完后用）。
- 旧接口 `/state`、`/events`、`/control` 的输出冻结，不因为这里的任何规则而改变，见第 9 节。
  例外（2026-09-26，用户批准）：`/state` 的 `sim` 加了只读字段 `spn`（卡里的服务商名，厂商存的 UCS-2 hex 解码后的文字，
  读不到是 `""`），给触屏区分 CMLink 和中国移动香港（IMSI 都是 454-12）。只加不改，旧字段一个字节都没变，golden 已重录。

## 1. 流的标识：epoch 和 seq

**V2-1** 每次 datad 启动随机生成一个 `epoch`（字符串）。`/v2` 的每条事件和 `GET /v2/state` 的响应都带它。
订阅方看到 `epoch` 和上一条不同，就断开重连。
测试（T5）：`v2_epoch_changes_on_restart`

**V2-2** `seq` 是整条流的全局计数（u64），只有广播事件（`block`、`heartbeat`）才分配：
每发一条就取下一个值。启动后第一条广播事件的 `seq` 是 1。snapshot 不占 `seq`（见 V2-5）。
测试（T5）：`v2_seq_only_block_and_heartbeat_increment`

**V2-3** 订阅方判断「不连续」只看一条：收到的 `seq` ≠ 上一条 + 1。心跳也占号，所以丢一条心跳同样算不连续。
发现不连续就断开重连，重连后的第一条事件是新的 snapshot。这是唯一的重新同步办法。
测试（T5）：`v2_seq_contiguous_with_heartbeats_between_blocks`、`v2_missing_heartbeat_detected_as_gap`

## 2. 连接和 snapshot

**V2-4** 新连接收到的第一条事件是 `snapshot`，里面是全部块的完整数据。snapshot 不占 `seq`，它的 `seq` 字段是**切点**：
拍快照那一刻已经分配出去的最后一个 `seq`，启动后还没发过广播事件时是 0。
订阅方期望的下一条是切点 + 1。
测试（T5）：`v2_snapshot_cut_then_next_is_plus_one`

**V2-5** 分配 `seq` 用的是同一把状态锁。新连接在这把锁里**先订阅 broadcast，再拍快照**：
切点之后的事件一定能从 broadcast 里收到，切点之前的事件一定已经体现在快照里，不丢也不重。
新连接不影响已有连接的 `seq`。
测试（T5）：`v2_new_subscriber_keeps_existing_seq_contiguous`、`v2_concurrent_subscribe_no_loss_no_dup`

**V2-6** `GET /v2/state` 返回和 snapshot 一样的内容（带 `epoch` 和切点 `seq`），只用于调试和一次性读取，
**不能**拿来做重新同步。重新同步只有 V2-3 那一条路。
测试（T5）：`v2_state_endpoint_matches_snapshot_shape`

## 3. 推送通道

**V2-7** `/v2/events` 用 `tokio::sync::broadcast`，容量 64（按实测可以调）。
每条事件只序列化一次，所有订阅者共用同一份 `Bytes`。
测试（T5）：`v2_subscribers_receive_identical_bytes`

**V2-8** 订阅者在 broadcast 里落后超过容量，收到 `Lagged`，datad 立即关掉它的 SSE 连接，不补发。
它重连后先收到新的 snapshot。其他订阅者不受影响，`seq` 照样连续。
测试（T5）：`v2_slow_subscriber_lagged_is_closed`、`v2_lagged_reconnect_gets_new_snapshot`、`v2_other_subscriber_contiguous_during_lag`

`/v2/events` 和旧 `/events` 共用同一个 SSE 连接上限（现在 16 个），满了同样回 `503`（`sse_client_limit`）。

**V2-9** 旧 `/events` 仍然用现有的 `watch` 通道，推完整快照，行为不变。
测试（T1）：`legacy_events_golden_unchanged`

## 4. 事件格式

SSE 事件名就是事件类型：`snapshot`、`block`、`heartbeat`。`data` 是单行 JSON。
不发 `retry:`（旧 `/events` 实际也不发，重连间隔由订阅方自己定；T1 的 golden 为证）。
不发 SSE 的 `id:` 字段，因为重连一律从 snapshot 开始，不支持按 `Last-Event-ID` 续传。

```text
event: snapshot
data: {"epoch":"5f2c9a1e","seq":41,"blocks":{"battery":{"revision":7,"observed_at":1782396733,"stale":false,"data":{...}},"charger":{...}}}

event: block
data: {"epoch":"5f2c9a1e","seq":42,"name":"battery","revision":8,"observed_at":1782396735,"stale":false,"data":{...}}

event: heartbeat
data: {"epoch":"5f2c9a1e","seq":43,"blocks":{"battery":1782396735,"charger":1782396735,"live":1782396736},"exec_age_ms":0}
```

**V2-10** 各字段含义：

- `block`：`{ epoch, seq, name, revision, observed_at, stale, data }`，只有一块。
- `snapshot`：`{ epoch, seq, blocks: { name: { revision, observed_at, stale, data } } }`，`seq` 是切点。
- `heartbeat`：`{ epoch, seq, blocks: { name: observed_at }, exec_age_ms }`；`exec_age_ms` 是执行者多久没前进（V2-32）。
- `observed_at`：这块最后一次**读成功**的时间，单位秒，时间基准和 `/state` 的 `ts` 相同（设备时钟）。
  读失败或 stale 时保留上一次成功的时间，不更新。
  设备时钟会被 SNTP 调整，订阅方判断「多久没收到」要用自己的单调时钟，不能用 `observed_at` 相减。

测试（T5）：`v2_event_shapes_match_doc`

块的 `data` 和旧 `/state` 里对应的子对象**同一形状**，消费方从旧接口迁过来只换数据来源：

| 块 | `data` | 来源 |
|---|---|---|
| `battery` | 旧 `/state` 的 `battery` 对象 | `zwrt_bsp.battery` 的回复 + 充电器块最近一次读成功的回复 + sysfs 电压电流 |
| `charger` | 旧 `/state` 的 `power` 对象；旧 `/state` 没有 `power` 时是 `{}` | `zwrt_bsp.charger` 的回复 |
| `signal` | 旧 `/state` 的 `net` 对象 | 旧采集算好的 `net`（不另调 ubus） |
| `live` | `{ "system", "runtime", "traffic" }`，三个值分别是旧 `/state` 的同名对象 | 旧采集算好的结果（不另调 ubus） |
| `sms` | `{ "unread", "max_id", "count" }`：`unread` 同旧 `/state` 的 `sms.unread`；`max_id`、`count` 是 `/v2` 新加的（V2-30）；不带 `list` | 旧采集读好的容量和两库第一页（不另调 ubus） |
| `sim` | 旧 `/state` 的 `sim` 对象（`iccid`、`imsi`、`spn`、`state`、`current_slot` …），2026-10-03 加；变了才发 | 旧采集读好的 `zwrt_zte_mdm.api get_sim_info`（不另调 ubus） |
| `op` | 写操作的界面数据（第 12 节，E4 T13）：`{ "rollback_enabled", "active", "last" }` | 事务引擎（不调 ubus） |

stale 时 `/v2` 保留旧值，旧 `/state` 仍按读失败输出（V2-29）。
`signal` 在 `nwinfo_get_netinfo` 失败时读失败；`live` 在 `system info` 或实时流量 `get_wwandst` 失败时读失败。

## 5. 块：revision、stale、max_age

**V2-11** 每块有自己的 `revision`（u64）。含义是**数据或健康变化了**：
`data` 和上次发布的不同，或者 `stale` 翻转了，才 +1，同时发一条 `block` 事件。
数据没变、健康也没变，就不涨，也不发。信号块和 live 块的发布时机另有规定（V2-15、V2-16），但涨不涨 revision 同样按这条判断。
测试（T4）：`block_revision_unchanged_when_data_same`、`block_revision_bumps_on_data_change`

**V2-12** 读失败（ubus 出错、超时、返回无效数据）时，这块**立即**置 `stale=true`，内部保留上一次的值，
`/v2` 里 `data` 仍然是那份旧值。失败或恢复导致 `stale` 翻转，就算数据完全不变，也 revision +1 并发 `block`。
所以「成功 → 失败 → 恢复成同样的值」会发两条 `block`。
测试（T4）：`block_stale_flip_publishes_twice_same_data`

**V2-13** 每块的 `max_age` = 3 × 这块的采集间隔，最少 15 秒；采集间隔是「每轮」的块按当前采样间隔算。
距上次读成功超过 `max_age` 还没有再读成功（比如一直被每轮预算挤掉，没轮到读），就置 `stale=true` 并按 V2-12 发布。
下次读成功时翻回 `false`，再发一条。
测试（T4）：`block_starved_to_max_age_goes_stale`、`block_stale_clears_after_recovery_read`

**V2-14** 启动后一直没读成功过的块：`stale=true`、`data=null`、`observed_at=0`、`revision=0`。
第一次读成功后 revision 变成 1。
测试（T4）：`block_never_read_is_stale_null`

## 6. 什么时候发布

**V2-15** 信号块（RSRP / RSRQ / SINR、小区、制式、频段）：

- **什么时候发由阈值决定**：RSRP、RSRQ、SINR 任何一项和**上次发布的值**相差 ≥ 1 dB，或者小区、制式、频段变了，就立即发布。
  比的是上次发布的值，不是上一次读数，这样缓慢漂移会累积到阈值被发出去。
- **30 秒强制发布**：距上次发布满 30 秒，发布一次，把小于阈值的变化也送出去。
  这时如果数据和上次发布的**完全相同**，就不发，也不涨 revision。
- 凡是发布了，并且数据和上次发布的不同，revision +1（V2-11）。
- `stale` 翻转照 V2-12 立即发布，不受阈值限制。
- 信号块的 `data` 是旧 `/state` 的 `net`。按 dB 比的是 `nr_rsrp`、`nr_rsrq`、`nr_snr`、`lte_rsrp`、`lte_rsrq`、`lte_snr`，
  以及 `nrca`、`lteca`、`ltecasig` 里每个载波的 RSRP/RSRQ/SINR；`bars`、各个 RSSI 自己不触发发布，随下一次发布带出；
  载波个数、载波的 PCI/频段/频点/带宽和 `net` 里其余字段（制式、频段、小区、运营商、漫游……）变了立即发布。

测试（T5）：`signal_drift_0_3db_publishes_once_at_30s`、`signal_unchanged_not_published`、`signal_1db_change_publishes_immediately`、`signal_cell_change_publishes_immediately`

**V2-16** `live` 块放 uptime、CPU、流量计数器、速率这类每秒都在变的字段，每轮结束时发布一次，不参与其他块的变化判定。
它的节拍就是采集轮的节拍：某一轮超出预算变长了，节拍跟着变慢，不另外补发。
测试（T5）：`live_block_published_every_round_end`、`live_block_cadence_follows_round_length`

**V2-17** 其余块：`data` 有任何变化就发布（V2-11）。

测试（T4）：`block_any_change_publishes`

## 7. 采集执行者、每轮预算、心跳

**V2-18** datad 里只有一个**采集执行者**任务发 ubus 请求，同一时间最多一个在途。
帧按 seq 和 peer 过滤；请求超时就重连，这个对象本轮跳过（算读失败，按 V2-12 置 stale）。
阶段 1 的 `ZWRT_DATAD_UBUS=cli` 后端也经过这个执行者。
测试（T3）：`ubus_late_reply_is_dropped`、`ubus_timeout_reconnects_and_skips_object`；测试（T4）：`executor_is_only_ubus_caller`

**V2-19** 采集循环：先睡一个采样间隔（`state.set_interval`，500～5000 ms），再采一轮。
单个 ubus 请求的超时是 2 秒。每轮 ubus 预算 3 秒，不算 `/control` 插进来的时间：
每次**开始**读下一个对象前，看本轮已经用掉的 ubus 时间，满 3 秒就不再开始新的请求。已经发出的请求照常等到回复或 2 秒超时。
所以一轮最长是 3 秒 + 一次超时 = 5 秒。
单个请求的超时按后端分：`socket` 是 2 秒；`cli`（`ubus call`，默认）保留原来的 8 秒（fork 开销大），
这时一轮最长是 3 + 8 = 11 秒。
2 秒只用于采集轮里的读取；`socket` 后端在采集轮之外（控制任务、内部任务）的请求用 8 秒，
因为写操作（比如切换设置）可能要等较久，2 秒就判超时会把已送达的写操作报成失败。控制任务不计每轮预算（见 V2-23）。
例：同一轮有 3 个对象都不回复。第 1 个 0～2 秒超时，第 2 个 2～4 秒超时，第 3 个没开始。这一轮约 4 秒，心跳照发，下一轮先读第 3 个。
测试（T4）：`round_three_timeouts_within_budget_plus_one_timeout`、`control_calls_use_control_timeout_rounds_use_round_timeout`、`socket_control_timeout_longer_than_round`

**V2-20** 轮转：本轮因为预算没轮到的块，下一轮从停下的地方开始、先读它们，保证慢对象不会一直把后面的块挤掉。
测试（T4）：`round_rotation_reads_skipped_blocks_first`

**V2-21** 每块自带采集间隔，取代现在的 `ubus_ttl`：
距上次读取满间隔才读；读失败的块 5 秒内重读，不等满间隔；`ZWRT_DATAD_CACHE=0` 时每块每轮都读。
`/control` 成功后，相关的块标成「立即读」，**下一轮**不管间隔到没到都读；动作没有对应的块时，全部块都标（V2-27）。
测试（T4）：`block_interval_respected`、`block_failure_retried_within_5s`、`block_cache_off_reads_every_round`

**V2-22** 每轮结束时（包括超出预算的那一轮）执行者发一条 `heartbeat`，没有任何变化也照发。
另有一个独立定时器（E4 D12，不经执行者）：距上一条心跳满 5 秒就补一条。所以执行者被长任务占着（比如短信发送、
原厂 reload 的短轮询）或者卡住时心跳照样有；订阅方看心跳里的 `exec_age_ms` 判断执行者是不是卡了（V2-32），不再只靠「心跳停了」。
测试（T4）：`heartbeat_sent_after_over_budget_round`；测试（E4 T3）：`long_reload_short_polls_keep_heartbeat_and_no_stall`、`idle_executor_is_never_stalled`

**V2-23** 所有 `/v2` 订阅方统一用 **M = 20 秒**：超过 20 秒没收到任何事件，就当作连接断了，按 V2-3 重连。
没有 `/control` 时，心跳最长间隔 = 5 秒采样间隔 + 5 秒最长一轮 = 10 秒，M 留出一倍余量。
10 秒这个上限只对 `socket` 后端成立；`cli` 后端一轮最长 11 秒（V2-19），心跳最长间隔 5 + 11 = 16 秒，仍小于 M = 20 秒。
`/control` 任务不算进每轮预算，控制请求很多时间隔可能超过 10 秒，这不是硬保证。
测试（T4）：`heartbeat_gap_at_most_10s_without_control`；测试（T7，manager）：`datad_feed_disconnects_after_20s_silence`

## 8. `/control` 排队

**V2-24** `/control` 不自己发 ubus，而是把请求作为一个**控制任务**交给执行者，
连接保持挂着，直到任务做完再回复（和现在一样，见 `CONTROL_API.md`）。
执行者每读完一块，先把排队的控制任务按先来后到做完，再读下一块：控制优先。
每个安全点只做**进入时已在排队的**任务（快照计数），做的过程中新来的留到下一个安全点；本机内部任务（登录校验等）同一规则。
所以持续有 `/control` 或内部任务进来，也挡不住采集轮和轮末心跳。
单个控制任务可能占住执行者较久（比如短信发送是 20 秒级），期间不发心跳，这是设计接受的。
测试（T4）：`control_runs_before_next_block`、`rounds_and_heartbeats_continue_under_control_flood`

**V2-25** 控制队列最多 8 个**排队中**的任务（正在执行的不算）。满了新请求立即回 `503`，不排队：

```json
{"ok":false,"action":"<action>","error":{"code":"busy","message":"control queue full"}}
```

例：采集进行中连续发 10 个 `/control`，前 8 个按顺序执行并回复，后 2 个立即收到 503。
测试（T4）：`control_queue_ten_requests_eight_ordered_two_busy`

**V2-26** 一个动作要连着调几次 ubus 的（比如 `apn.list`），在同一个任务里依次调完，中间不插入采集。
测试（T4）：`control_multi_call_runs_as_one_task`

**V2-27** `/control` 成功后只把相关的块标成「立即读」（V2-21）：下一轮立即读它们，动作没有对应的块时全部块都读；**不**另起一轮采集。
测试（T4）：`control_success_marks_blocks_no_second_round`

**V2-28** 所有读写都经过同一个执行者，按顺序执行，所以在控制任务之前读到的旧值，不会覆盖控制任务之后的新值。
测试（T4）：`control_result_not_overwritten_by_older_read`

## 9. 旧接口

**V2-29** 旧的 `/state`、`/events` 和 `/v2` 从同一份状态生成。遇到 stale 的块，按今天读失败时的样子输出（字段缺失或为 `-1`），
不输出保留的旧值。只有 `/v2` 才看得到 `stale` 和保留的旧值。
测试（T4）：`legacy_state_stale_block_renders_as_failure`

## 10. 短信

**V2-30** `sms` 块是短信摘要，用来告诉订阅方「有没有新短信」，不带短信内容：

- `unread` = `sms_dev_unread_num` + `sms_sim_unread_num`（和旧 `/state` 的 `sms.unread` 相同）。
- `max_id` = NV（`mem_store=1`）和 SIM（`mem_store=0`）两库降序第一页里最大的编号（含已发送、草稿；两库共用一个编号计数）。
- `count` = 两库 `sms_{nv,sim}_{rev,send,draftbox}_total` 之和（B27 的字段；`sms_nvused_total` 实测不可信，不用）。
- 容量和两库第一页三次读取都成功才算读成功，任一失败按 V2-12 置 stale。节拍沿用旧采集读短信的节拍（容量 30 秒、列表 10 秒），不另调 ubus。
- 发布按 V2-17（有变化就发）。

要短信本身用 `/control` 的 `sms.list_after {after_id, limit}`（`limit` 1～50，默认 50）：返回编号 > `after_id` 的前 `limit` 条（升序）和 `has_more`。
固件的 `order_by` 只接受 `"order by id desc"`，所以 datad 两库各自降序翻页（每页 50 条），读到编号 ≤ `after_id`、短页或没有新编号就停，
合并去重后升序取前 `limit` 条；每库最多翻 20 页，超过或任一库任一页读失败，整次失败（`502`），不返回半截。
某库一个满页的编号集合和上一页完全相同（固件没理 `page`），也整次失败，不当成翻完（否则会漏更早的新短信、`has_more` 误报 false）。
每次调用的 ubus 次数 = 每库 ⌈(该库编号 > `after_id` 的条数 + 1) / 50⌉。它是一个控制任务（V2-24、V2-26），只读：不清慢数据缓存、不标块。
条目字段和 `zte_libwms_get_sms_data` 相同（`id`、`number`、`content`、`date`、`tag`），号码和正文是解开厂商信封后的 UCS-2 hex。
订阅方在 `max_id`、`count` 变化或来源切换时按 `after_id` 翻页到 `has_more=false`。
测试（T10）：`sms_block_summary_fields`、`sms_list_after_pages_desc_only`、`sms_list_after_merges_stores_dedup`、`sms_list_after_limit_has_more`、`sms_list_after_one_store_fails_whole`、`sms_list_after_page_ignored_fails_whole`、`sms_list_after_returns_plain_fields`；测试（T10，manager）：`sms_burst_600_paged_forwarded_once`、`sms_interrupted_on_page_3_resumes`、`sms_datad_outage_120_backfilled`、`sms_list_after_busy_retried_over_http`

**V2-31** 短信事件：datad 自己起一个长期运行的 `ubus listen zwrt_wms_status_event` 子进程，逐行读它的输出（一行 JSON，顶层键是事件名）。
收到事件后等 300 ms（这期间再来的事件并成一次）；离上次触发不到 2 秒就等到满 2 秒（期间的事件也并进这一次），短信密集时不会每 300 ms 整轮重采。然后把短信容量、短信列表的慢数据缓存清掉，`sms` 块标成「立即读」，
并唤醒执行者：下一轮不等采样间隔立即开始；正在跑一轮时，这一轮跑完马上再跑一轮。轮本身照旧在执行者里串行跑，轮里照旧控制优先（V2-24），不另起并发的采集。
监听是独立的订阅连接，只收事件、不发请求，所以不算执行者之外的 ubus 调用，也不违反「同一时间最多一个在途」（V2-28）：短信本身仍由执行者读。
子进程退出或起不来就退避重启：1 秒起、每次翻倍、最多 30 秒，连续跑满 60 秒后退避回到 1 秒；日志只在第一次退出和退避到顶时各写一行。
只用子进程方式（`ZWRT_DATAD_UBUS=socket` 时也是），直连 ubusd 的订阅等 socket 后端上机（Gate 0）后再做。`ZWRT_DATAD_SMS_LISTEN=0` 关闭，默认开。
例：新短信到达 → 事件 → 300 ms 后开一轮 → 约 1 秒内 `sms` 块的 `max_id` 变化发到 `/v2`（原来最多等 10 秒列表缓存）。
测试（T10）：`sms_event_updates_block_within_one_round`、`sms_events_coalesced`、`sms_event_kicks_at_least_2s_apart`、`sms_listener_restarts_after_exit`、`sms_listen_disabled_by_env`、`sms_event_invalidates_only_sms_cache`

## 11. 活性和写操作的闸（E4 T3）

**V2-32** 执行者的「前进」= 完成一次 ubus 调用、一个任务、一轮，或者闲着在等活（闲着时 `exec_age_ms` 是 0）。
心跳里的 `exec_age_ms` 是最近一次前进距今的毫秒数。订阅方（触屏、agent）超过 **20 秒**就当 datad 卡了：触屏显示「数据服务没响应」、不写，agent 切退路（D12）。
datad 自己有看门狗（独立系统线程，每秒看一次）：超过 **30 秒**没前进、而且不是一个还在它自己超时里的调用（单次调用上限 10 秒），就记一行退出，由 procd 拉起。
`ZWRT_DATAD_WATCHDOG_S` 改上限（秒，0 = 关），不会小于 11 秒。所有 ubus 和子进程调用的超时都必须小于它；
原厂要拖几十秒的动作（`zwrt_wlan reload` 这类）先发、再用多次短调用轮询，不能在一次调用里等完。
datad 启动时把 pid 写进 `ZWRT_DATAD_PID_FILE`（默认 `/var/run/zwrt-datad.pid`），应急直写脚本据此判断 datad 在不在（D18）。
测试（E4 T3）：`stuck_call_is_reported_and_stalls_after_limit`

**V2-33** 写的上下文（拿着跨进程写锁的那段：事务的写和退回、会改设备的 `/control`）里调用超时了，结果算「未知」：原厂那边可能还在做（D28）。
执行者先把闸关上：下一个写的调用之前，先用这个对象的只读请求探测（固定表：`zte_nwinfo_api` 用 `nwinfo_get_netinfo`、`zwrt_data` 用 `get_wwaniface` 等；
表里没有就用块表里读它的请求，再没有就是 `system board`），每次最多 8 秒、最多 4 次，对象回答了（成功或报错都算）才放行；
4 次都超时也放行并记一行，写的结果交给事务按读回判断。
写之外的调用（采集轮、事务确认时的读、只读的 `/control`）超时不关闸、也不等闸，免得确认期间整个执行者陪着探测。写事务遇到超时不判失败，只按读回判断。
跨进程写锁 `ZWRT_DATAD_WRITE_LOCK`（默认 `/var/run/u60-write.lock`，flock，D29）：datad 的每个写和启动时处理 takeover/pending 的全过程都拿着它，
和应急直写脚本互斥；拿不到就等，满 15 秒还拿不到就记一行照做（小于订阅方判卡死的 20 秒）。
测试（E4 T3）：`timeout_holds_next_call_until_object_answers`、`gate_opens_after_four_failed_probes`、`round_timeouts_do_not_close_the_gate`、`read_timeouts_do_not_close_the_gate`、`probe_uses_the_vendor_read_for_written_objects`、`timed_out_write_is_unknown_and_confirmed_by_readback`

## 12. 写操作的界面数据（E4 T13）

设计见 manager `docs/designs/write-op-layer.md` 的设计评审（DD3、DD6、DD7、DD10、DD12、DD16）。文字由 datad 按「状态文案表」发，触屏和网页照着显示，不自己拼结论。
本节说的「事务视图」就是 `op.status` 回复、提交回复里的 `op`、`op` 块的 `active`/`last`：在原来的状态字段上加下面这些。

**V2-34** `/v2` 加一块 `op`（派生块，不调 ubus）：`{"rollback_enabled": 布尔, "active": 事务视图或 null, "last": 事务视图或 null}`。
`active` 是进行中的事务；`last` 是最近一个结束的事务，另带 `acked`（有人点过「知道了」）和 `needs_ack`（常驻类结果且还没点）。
每轮采集交一次，事务引擎每次状态变化（新事务、读数、终态、退回/保留、「知道了」）也马上交一次；两条路都在引擎的锁里算好再交，旧的不会盖掉新的。
进行中时 `remaining_ms` 每轮都变，所以这块每轮都发一次（新订阅的也拿到准的倒计时）；没有进行中的事务时数据不变、不发。datad 卡住时它和别的块一样变 stale。
`/v2/screen` 顶层也带同一份 `op`（按请求那一刻算），触屏不用订阅 `/v2/events`。
测试：`op_block_follows_the_engine`、`op_block_is_quiet_without_a_change`

**V2-35** 事务视图加的字段（文字都有 `_zh`/`_en` 两份；值的显示名按项转，网络模式用首页同一张表）：
- `source_zh/_en`：来源显示名（DD14：screen 触屏/Screen、web 网页/Web、scenario 情景/Scene、scheduler 定时任务/Schedule、auto 自动/Auto、guard 自动恢复/Auto-recovery、legacy 触屏/Screen）。
- `what_zh/_en`：改的是哪一项（制式/Network mode）；`old_zh/_en`、`target_zh/_en`、`rollback_to_zh/_en`、`readback_zh/_en`（没有读回为 null）。
- `say_zh/_en`：一句结论（进行中：正在换制式 / 正在确认 / 正在退回；终态按文案表，不带 ●▲■）；`mark`：`ok`/`warn`/`bad`（进行中为 null），客户端自己画符号。
- `stay`：`live`（进行中）、`brief`（显示 3 秒）、`sticky`（常驻到「知道了」）、`alert`（退回也没通，常驻并占首页状态块，DD9）、`none`（被覆盖，不单独提示）。
- `next_zh/_en`：倒计时那行，`{t}` 是占位，客户端按 `remaining_ms` 换成 `m:ss`（自动退回开：「{t} 后没通就退回到 X」；关：「还剩 {t} · 自动退回没开」；退回中：「退回到 X · 还剩 {t}」）；没有为 null。
- `note_zh/_en`：「重启过 · 重新确认」（写之后整机重启过、还在等），否则 null。
- `steps`：三行进度 `[{"key":"applied"|"registered"|"data","zh","en","done"}]`（设置已生效 / 已注册 / 数据）；退回中看的是退回目标。
- `can_revert`、`can_keep`：现在点「退回 X」「保留 Y」有没有用（在等确认、没有在途的写）；按钮文字 `revert_label_zh/_en`（退回自动 / Revert to Auto）、`keep_label_zh/_en`（保留只用 4G / Keep 4G only）。
- `undo`：进行中为 null；结束了是 `{"ok", "label_zh/_en"（撤销/重做）, "why_zh/_en"（不能撤时的原因）, "value"}`（V2-36）。
英文都是 ASCII、没有句号；首页大字 ≤ 10 个字符，所以进行中的英文不带省略号（Switching / Checking / Reverting）。
`brief` 的结果只在这次连接里亲眼看到它结束时显示 3 秒（这个 op_id 在 `active` 里出现过，或者 `last.op_id` 在连接期间变了）；刚连上、刚重启拿到的第一份里的 `brief` 不显示（块里不带结束了多久）。
测试：`every_text_row_has_zh_and_en_within_budget`、`steps_follow_readings`

**V2-36** 能不能撤销（DD10，按动作 + 终态）：confirmed、unverified 能撤；rolled_back、not_applied 不能（「设置没变 · 不用撤销」）；rollback_failed 不能（用「再试一次退回」）；
cancelled 里 sim_changed 不能（「换过卡」）、superseded 不能（「之后又改过」），其余在读回还是目标值时能撤，否则「之后又改过」。同一项后来又有别的事务的，旧的一律「之后又改过」。
撤销就是用 `value` 发一个带 `undo: true` 的新写；撤销过的再点叫「重做」。
测试：`undo_by_phase_and_reason`、`later_change_blocks_undo`

**V2-37** `op.ack`（「知道了」，DD16）：只记账，两边共享。要顶层 `source`（screen 或 web）和 `params.op_id`；只能点 `last`，点过再点照样回成功（不重复记账）。
`last` 和 `acked` 落盘在 `ZWRT_DATAD_OPS_DIR` 的 `last.json`，datad 重启后结果还在、点过的不再出现。同一项下一次结束的事务自然接替 `last`。不受事务锁、不进执行者。
测试：`ack_is_shared_and_journaled`、`last_result_survives_restart`

**V2-38** 首页卡的结论放在 `/v2/screen` 的 `net.home`（只在有写操作要叠时出现，没有就用 `net.story`）。`net.story` 永远只是网络结论：agent 的 netwatch 历史和深查读它，写操作不能把它们带偏。
`home` 比 `story` 多两档，排在「无 SIM」「移动网络已关」之后、「只能紧急呼叫」「无服务」「没连上网」之前
（换制式时常会先经过无服务，DD3 和用户旅程第 3 步要的是不出红色）：
- `changing`：有进行中的事务。中性色；大字是 `say`（正在换制式 / 正在确认 / 正在退回），提示是倒计时那行（`{t}` 已换好）。
- `revert_fail`：`last` 是退回也没通、还没点「知道了」。红色；大字「退回也没通」/`Failed`，提示写现在的值（读不到写「当前设置未知」）、上次确认的值和下一步。
其他常驻类结果还没点「知道了」时，大字照旧是网络结论，提示末尾加一句结果（DD16）。没有要叠的就没有 `home`，`/v2/screen` 和以前逐字段一样（对照样本不变）。
旧触屏的缓冲（回复 16 KB、`net` 8 KB）在最坏情况下（最长的值、进行中 + 退回也没通、重启过）也放得下。
测试：`story_changing_beats_no_service_and_offline`、`story_revert_fail_takes_the_card`、`story_sticky_result_rides_on_the_hint`、`response_with_op_fits_the_old_screens_buffers`

**V2-39** busy 回复的 `doing` 加 `say_zh/_en`：「正在换制式（触屏发起，32 秒），稍等」/「Busy: network mode (Screen)」；搜网会话是「正在搜网」/「network search」。
测试：`busy_reply_says_who_and_what`

**V2-40** `/v2/screen` 顶层带 `exec_age_ms`（同心跳里的，V2-32）。触屏只轮询 `/v2/screen`、看不到心跳；这个请求读现成的快照、不经执行者，datad 卡住时照样回。
触屏超过 20 秒就显示「数据服务没响应」、写控件变灰；没有这个字段（旧 datad）当不知道，不当卡住。
测试：`screen_reply_carries_executor_age`

不在 datad 的几行（客户端自己判断的）：「数据服务没响应」（心跳 `exec_age_ms`，V2-32）、「和设备断开了 · 操作结果未知」（网页）、改动记录的空和读不到，归触屏 T8、网页 T9 和术语表 T14。
