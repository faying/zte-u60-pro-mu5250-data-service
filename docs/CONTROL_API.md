# Device Control API

控制接口只用于 UFI 与 datad 的本机服务间通信。

```http
POST /control
Authorization: Bearer <token>
Content-Type: application/json
```

统一请求：

```json
{"action":"<action>","params":{}}
```

2026-10-06 删掉了 57 个没人调用的动作（设备上计数约 15.5 小时为零，触屏、agent、网页、脚本都没有引用；清单见 manager `docs/designs/legacy-api-removal.md`），现在回 404 `unknown_action`，访问照样记进 `/debug/legacy-hits`：
`device.login/login_info/logout/session_status/change_password`、`wifi.status`、`wifi.dual_band_status`、`wifi.set_dual_band`、`wifi.set_chip`、`wifi.configure`、`wireless.config`、`wifi.txpower.*`、`wifi.advanced.status`、`wifi.psm.set`、`wifi.interface.*`、`sleep.*`、`usb.status`、`power.direct_supply.status`、`apn.list`、`client.*`、`neighbor.*`、`state.refresh`、`qos.reload`、`qos.clear`、`traffic.*`、`multiwan.*`、`aggregation.set`、`cooling.*`、`cellular.connect/disconnect`、`cell.unlock_all`、`sim.set_slot`、`lan.set_mtu`、`dns.set`、`modem.airplane`。

## Device

| action | params | 说明 |
|---|---|---|
| `device.reboot` | 无 | 重启设备 |
| `device.poweroff` | 无 | 关闭设备 |

UFI 自己的登录口令、HTTP 签名和浏览器会话不属于这里。

## Cellular And Radio

| action | params |
|---|---|
| `cellular.set` | `enabled?`, `roaming?`, `connect_mode?` |
| `network.set_mode` | `mode` |
| `band.set_lte` | `bands`，逗号分隔；空串表示自动 |
| `band.set_nr_sa` | `bands` |
| `band.set_nr_nsa` | `bands` |
| `cell.lock_lte` | `pci`, `earfcn` |
| `cell.lock_nr` | `pci`, `arfcn`, `band` |
| `band.reset` | 无；原厂 `nwinfo_reset_band_cell_setting`，频段和小区锁定全部恢复默认 |

`cellular.set` 会先读取完整 `get_wwaniface` 对象，再覆盖调用方提供的字段，避免固件清空未指定的 PDP、配置档案等属性。

## 事务（E4 写操作层）

设计见 manager `docs/designs/write-op-layer.md`。目前只有 `network.set_mode` 走事务（`rust/src/ops/spec.rs` 的描述表），其余动作照旧。

**新客户端**在请求顶层带 `source`（`screen`、`web`、`legacy`、`guard`、`scenario`、`scheduler`、`auto`），可选 `op_id`（1–64 个字母、数字、`.`、`_`、`-`；同一个 op_id 重发只回现有状态）和 `undo`：

```json
{"action":"network.set_mode","source":"screen","op_id":"screen-42","params":{"mode":"Only_LTE"}}
```

- 写之前读当前值和 SIM（完整 ICCID + 卡槽），写之后按新鲜读数确认：配置读回 = 目标值且已注册；「应当有数据」（`get_wwaniface` 数据开关开，且不在漫游或漫游开关开）时还要数据通：`connect_status` 已连接、`zte_wan` 有 IPv4，并且一次绑定蜂窝接口（`get_wwaniface` 的 `ipv4_dev_name`）的 DNS 查询有回答（问运营商 DNS；连接没有 IPv4 时（D41）改看 `zte_wan6`：有 IPv6 地址就用它当连接身份，绑定 `ipv6_dev_name`（空时用 `zte_wan6` 的 `l3_device`）向前两个 IPv6 DNS（`zte_wan6` 的 `dns-server`，没有时用 `get_wwaniface` 的 `ipv6_dns_prefer`/`ipv6_dns_standby`）发 AAAA 查询，同样绝不发不绑定接口的查询；一轮最多失败 3 次、间隔至少 5 秒，一轮都失败后同一条连接隔 30 秒再来一轮，换了连接马上重新计；通了就停）。不应当有数据时不发任何探测。退回也按同一条规则确认。成功回 200 `{"ok":true,"action":…,"result":<原厂回复>,"op":<状态>}`；写调用报错回 502，`op` 照样带上（事务按读回判断）；写之前读不到当前值回 502，没动设备。
- 同一时刻只有一个进行中的事务。别的写回 **409** `{"error":{"code":"busy"},"doing":{op_id,action,source,phase,age_ms}}`。能插队的：同一项的用户写（screen/web/legacy）和 guard（旧事务记 `superseded`，新事务的退回目标继承旧事务的）、关数据/关漫游（`cellular.set` 只含 `enabled`/`roaming` 且都为关，旧事务记 `preempted`）。
- **确认中的其他写**（D40，STATE_V2.md V2-42）：事务在进行中时，影响上网的写（会话期间收 409 的那些，描述表里的动作除外；加上 `netselect.auto`、`cellular.redial`，以及 `vendor.call` 的 STC 小区锁和 SIM PIN/PUK/NCK）按来源处理：用户的（`screen`、`web`、没有 source 的旧请求）照做，同时取消这次自动退回，事务记 `cancelled/other_change`（旧请求的回复逐字节不变）；自动来源（`guard`、`scenario`、`scheduler`、`auto`）不做、不记账，回 **409** `{"ok":false,"action":…,"error":{"code":"op_busy","message":"a change is being confirmed; try again after it ends","op":{"op_id","item","phase"}}}`，等事务结束再发。关数据/关漫游照旧插队（`preempted`）。
- 到点没确认：自动退回默认关（`ZWRT_DATAD_ROLLBACK=1` 才开），关着时以 `unverified/no_rollback` 结束；读回从没变成目标值以 `not_applied/ignored` 结束（不退回）。
- 状态（`op`）：`phase` 为 `accepted`、`applying`、`verifying`、`rolling_back` 或终态 `confirmed`、`unverified`、`rolled_back`、`not_applied`、`rollback_failed`、`cancelled`；`reason` 见设计稿「状态表」，另有 `sim_changed`（D32）、`reboot_loop`（D13）、`other_change`（D40：确认中用户又改了别的影响上网的设置，或 agent 发了 `op.interrupt`）。`ever_matched` = 读回对上过目标值（对上过、数据一直不通，到点按没通处理，不算 `not_applied`），`data_ok` = 数据这一关过了。
- 界面用的字段（`say_zh/_en`、`steps`、`undo`、`next_zh/_en` …）、`/v2` 的 `op` 块和首页「进行中」档见 STATE_V2.md 第 12 节（E4 T13）。busy 的 `doing` 也带 `say_zh/_en`。
- `op.*`、`journal.*` 和会话、E4 新动作不进 `/capabilities`：它属于冻结的旧接口（回复要逐字节不变）。（2026-10-06：删动作时 `/capabilities` 跟着改了，`control` 现在 25 项、`events` 为 `["snapshot","block"]`；这几类照旧不进。）新客户端看 `/v2/screen` 有没有 `op` 判断 datad 支不支持。

| action | params | 说明 |
|---|---|---|
| `op.status` | `op_id?` | 指定的，或当前/最近结束的事务；没有为 `null` |
| `op.revert` | `op_id` | 立即退回（自动退回关着也能用）；不在等确认时回 409 `invalid_state` |
| `op.keep` | `op_id` | 保留现状，取消退回，记 `confirmed/user_keep` |
| `op.ack` | `op_id` | 「知道了」：只记账，要顶层 `source`（screen/web）；只能点最近结束的那个（`op` 块的 `last`），点过再点照样成功。别的 op_id 回 409 `invalid_state`（STATE_V2.md V2-37） |
| `op.interrupt` | `what`（如 `esim`、`at`） | agent 里不经 datad 的用户写之前发（D40）：要顶层 `source`（screen/web，否则 400）；有进行中的事务就取消（`cancelled/other_change`，流水账另记一行带 `what`），回 `{"interrupted":true,"op_id":…}`；没有回 `{"interrupted":false}`（STATE_V2.md V2-43） |
| `op.notice_ack` | 无 | 「自动退回已打开」的提示点「知道了」（DD18）：要顶层 `source`（screen/web，否则 400）；回 `{"notice":"rollback_on","acked":true}`，记在 `ZWRT_DATAD_OPS_DIR` 的 `notice.json`，之后 `op` 块不再带 `notice`（STATE_V2.md V2-44） |

| `journal.append` | `item` 或 `action`、`result`，可选 `reason`、`old`、`new`、`detail` 等 | 只记账（eSIM、CHILL 这类不经 `/control` 的改动由 agent 补记）；要顶层 `source`；`result` 为 `skipped` 时按下面的规则合并。不受事务锁、不进执行者队列 |
| `journal.list` | `limit?`（默认 50，最多 500） | `{"entries":[…新的在前],"owners":{项:{source,user,undo,value,op_id,ts,t}}}`；每行另带界面显示用的 `what_zh/_en`、`change_zh/_en`、`result_zh/_en`、`mark`、`source_zh/_en`、`hide`、`undo_view`（STATE_V2.md V2-41） |

**搜网会话**（E4 T7b，D17）：agent 的搜网 / 手动注册 / 回自动流程留在 agent，期间占住写锁。

| action | params | 说明 |
|---|---|---|
| `netselect.session.open` | — | 要顶层 `source`；回 `{session, max_ms, lease_ms}`。有事务或会话进行中回 409 `busy` 带 `doing` |
| `netselect.session.renew` | `session` | agent 至少每 60 秒续一次，不续就当 agent 不在了、收回；最长 7 分钟 |
| `netselect.session.close` | `session`、`result?` | 放锁；会话号不对或已结束回 409 `invalid_state` |

会话期间：影响上网的写（`cellular.set`、`network.set_mode`、锁频、锁小区、APN、`modem.online`、`apn.set_pdp_type`）收 409，`doing.action` 是 `netselect.session`；描述表里的旧请求排队；其他写（短信、USB……）照常。会话结束（关、到点 `expired`、不续 `agent_gone`）各记一行流水账。

会话里的步骤（要顶层 `source`；`netselect.scan`、`netselect.register` 还要顶层 `session` = 当前会话号；`netselect.auto`、`cellular.redial` 带会话号算会话里的一步，不带也能做，会话结束后 guard 退回就是这样）。这几个和 `modem.online`、`apn.set_pdp_type` 先不进 `/capabilities`：

| action | params | 原厂 |
|---|---|---|
| `netselect.scan` | — | `zte_nwinfo_api nwinfo_manual_scan` |
| `netselect.register` | `mcc_mnc`（5–6 位数字）、`rat?` | `nwinfo_manual_register {m_mcc_mnc, m_rat}` |
| `netselect.auto` | — | `AT+COPS=0`（原厂没有能用的 ubus 调用） |
| `cellular.redial` | `type?`（1 IPv4、2 IPv6，不给两条都拨） | `zwrt_qcmap_cli set_qcliiface` |
| `modem.online` | — | `AT+CFUN=1`（`nwinfo_set_mode ONLINE` 拉不回 LPM）。B31 上两条都拉不回来（10-05 真机）：AT 口在 LPM 下不回话，25 s 后报错；只能重启 |
| `apn.set_pdp_type` | `ipv6`（布尔） | 拨号 APN 的 PDP 类型改成 IPv4v6 / IPv4（其他字段照原样），再拉起或断开 IPv6 那条腿 |

Wi-Fi（E4 T7b，zte-agent 的 Wi-Fi 页、热点开关、情景、家庭模式扫描用）：

| action | params | 说明 |
|---|---|---|
| `wifi.apply` | `set`（uci 路径 → 值，1–32 项）、`reload?`（默认 true）、`best_effort?` | 只认 `wireless.{main,guest}_{2g,5g}.{ssid,key,encryption,hidden,isolate,disabled,guest_active_time}`、`wireless.<射频>.{country,channel,txpowerpercent,htmode,disabled}`（射频 = `wifi0`/`wifi1`，或 `wireless.main_<频段>.device` 里写的名字）。值和 uci 一样也照写，commit 一次，再 reload 一次（agent 靠「总是写 + reload + 自己轮询 hostapd」重试修复，不看这里的回复判断成没成）。`best_effort` 时设不上的项跳过并列在 `skipped`。旧 agent 发的 `zte_mbb.wifi.{wifi_onoff,wifi6_switch}` 是死路径（没有 `zte_mbb` 这个包，原厂开关在 `wireless.zte_mbb`、要经 `zwrt_wlan set` 改）：照收、从不写、总列在 `skipped`。回 `{committed, skipped, reloaded, reload_error}` |
| `wifi.reload` | — | 只 `zwrt_wlan reload` |

其余原厂设置（E4 T7c，zte-agent 的路由、SIM PIN、STC、省电、快速开机、充电宝、自动休眠、恢复出厂、原样发短信）：

| action | params | 说明 |
|---|---|---|
| `vendor.call` | `object`、`method`、`args?`（对象，≤ 8 KB） | 只做 `control.rs` 的 `VENDOR_CALLS` 表里的 (对象, 方法)，`args` 原样交给原厂（和 agent 以前直接调的一样）。流水账里 PIN/PUK/NCK 类字段写 `(changed)`，短信方法不记参数；恢复出厂、`system reboot` 先记 requested 并落盘再做。会话期间不挡（短信转发不能被挡）。FOTA 相关的永远不进表 |
| `sms.db_delete` | `ids`（数字和 `;`） | 原厂 `zwrt_wms_delete_sms` 删不掉 SIM 里的短信时，agent 用它在原厂的 sms.db 里直接删（固定 SQL）。不记参数 |
| `dns.doh` | `enabled`（布尔） | agent 的 DoH：写 / 删 `/tmp/dnsmasq.d/doh.conf`（转发到 127.0.0.1:5353），关的时候再去掉 `dhcp.lan_dns` 的 server/noresolv，重启 dnsmasq |
| `wifi.power_save` | `enabled`（布尔） | MU5250 的 Wi-Fi 节能（触屏和网页共用）：写 `/etc/hotplug.d/iface/99-disable-powersave`（ifup 时对 wlan0–3 套用，留得住），删掉旧的 `psm`，马上对 wlan0–3 `iw set power_save`，再读回 wlan0（没有就 wlan2）。回 `{enabled, saved, live}`，Wi-Fi 关着读不到时 `live` 为 null；读回和要的不一样算失败 |

AT 只发这两条固定命令。AT 口和 zte-agent 共用，两边都拿 `ZWRT_DATAD_AT_LOCK`（默认 `/var/run/u60-at.lock`，flock）；口是 `ZWRT_DATAD_AT_PORT`，没设就按 agent 的顺序找第一个回 OK 的。等到 OK/ERROR 就停，最多 6 秒。

**流水账**（T5）：`ZWRT_DATAD_OPS_DIR` 下的 `journal.jsonl`，每行一个 JSON，带 `ts`（unix 秒）和 `t`（设备时钟的年月日时分秒，设备时钟本来就是当地时间）。会改设备的 `/control` 动作都记一行（`sms.mark_read` 不记）：事务结束时记 op_id、来源、SIM（ICCID 后 4 位/卡槽）、旧值、新值、退回目标、读回、终态和原因；不走事务的写记动作、来源（没有 source 记 `legacy`）、参数、`ok`/`failed` 和 HTTP 状态；旧请求队列的排队、被替换、丢掉也各记一行；重启、关机在执行前先记 `requested` 并等它写进闪存。密码类字段（Wi-Fi 密码、APN 用户名/密码、eSIM 激活码/确认码、PIN 等）只写 `(changed)`；ICCID、EID、IMSI、号码类字段只留后 4 位；短信动作不记参数。同一来源 + 同一项 + 同一原因的 `skipped` 只记开头一行（`skip:start`）和结束一行（`skip:end`，`count` = 一共跳过几次；原因变了，或这个来源对这一项有了别的记录时结束；计数在内存里，datad 重启时进行中的那段丢掉）。文件超过 `ZWRT_DATAD_JOURNAL_MAX_BYTES`（默认 262144）就改名为 `journal.1.jsonl`，两份合计不超过约 2 倍上限；单行最长 2 KB。`owners.json` 记每一项最后是谁写的（screen/web/legacy 的 `user` 为 true），流水账滚掉也不丢。

**旧请求**（没有 `source`）回复和以前逐字节相同。事务进行中：同一项的旧请求当覆盖写照常执行；描述表里的其他旧请求回成功（`{"result":"success"}`）并进旧请求队列，锁空出来再执行（每项只留最新、120 秒过期、关数据或重启时清空）。描述表里的动作在执行者队列满时也这样处理，不回 503；旧的关数据请求在队列满时作为内部任务马上执行。

进行中的事务落盘在 `ZWRT_DATAD_OPS_DIR`（默认 `/data/u60-ops`，空串 = 不落盘）的 `pending.json`：datad 重启接着确认；整机重启后时限重新计，第 2 次开机仍未确认或累计等待超过时限就马上退回；目录里有 `takeover` 标记（应急直写留下的）就放弃。写调用超时也回 502，但事务不判失败，只按读回判断（STATE_V2.md V2-33）。datad 的每个写都拿着跨进程写锁 `ZWRT_DATAD_WRITE_LOCK`（默认 `/var/run/u60-write.lock`），和应急直写脚本互斥。其他环境变量：`ZWRT_DATAD_OP_POLL_MS`（确认时读设备的间隔，默认 2000）、`ZWRT_DATAD_LEGACY_TTL_MS`（默认 120000）、`ZWRT_DATAD_DEADLINE_NETWORK_MODE_MS`（默认 120000）。

## WiFi And LAN

| action | params |
|---|---|
| `wifi.set_module` | `enabled`，`0/1`。整个 Wi-Fi 的原厂总开关（`wireless.zte_mbb.wifi_onoff`），按原厂网页的写法调 `zwrt_wlan set {"zte_mbb":{"wifi_onoff":"0"/"1"}}`；打开时带上当前的 `lbd`（双频合一），读不到就不带 |
| `lan.set` | `ip/netmask/dhcp_disabled/dhcp_start/dhcp_end/lease_seconds` |

## APN

| action | params |
|---|---|
| `apn.set_mode` | `mode`，设备侧整数 |
| `apn.add` | `name`, `apn`，以及可选认证字段 |
| `apn.modify` | `profile_id`, `name`, `apn`，以及可选认证字段 |
| `apn.delete` | `profile_id` |
| `apn.enable` | `profile_id` |

认证字段为 `username/password/auth_mode/pdp_type/roaming_pdp_type`。

## USB And NFC

| action | params |
|---|---|
| `usb.set` | `mode/port_switch/network_protocol` |
| `nfc.set` | `enabled`, `flag?` |

## Sampling

| action | params |
|---|---|
| `state.set_interval` | `milliseconds`，`500..5000`；运行时切换全局采样/SSE 推送周期，不重启 datad |

## SMS

| action | params |
|---|---|
| `sms.send_raw` | `number`, `message_hex`, `sms_time`，可选 `sender` |
| `sms.delete` | `ids`，使用设备要求的分号格式 |
| `sms.mark_read` | `ids`, `tag?` |
| `sms.list_after` | `after_id?`（默认 0）, `limit?`（1～50，默认 50）；只读，返回 `{items, has_more}`，见 `STATE_V2.md` V2-30 |

`sms.send_raw` 接受已经编码的 UCS-2 hex。`sender` 为空或 `host` 时使用当前主卡；TopFlow 可选 `x75`、`v3e1`、`v3e2`，其中 V3E 通过各自内网管理接口发送；普通双卡机型可选 `sim1`、`sim2`，datad 会先用原厂 provisioning 接口激活目标卡槽并等待切换完成。主机 WMS 发送会使用 datad 已注册的厂商 AES-GCM Web 会话加密号码和正文，并轮询 `sms_cmd=4`，只有状态 3 才返回成功。文本编码、转发、黑名单和业务去重继续由 UFI 负责。

## Safety

- 所有动作必须存在于编译期白名单。
- 服务名和方法名不能由请求指定。
- `ubus/uci` 使用 `fork/exec` 参数数组执行，不经过 Shell。
- WiFi、APN 和密码字段不得写入运行日志。
- 重启、关机应由 UFI 再做用户确认。

## Charger direct supply

| action | params | Result |
|---|---|---|
| `power.direct_supply.set` | `enabled`: boolean or 0/1 | Same fields plus `changed`, `verified` |

The adapter reads `zwrt_bsp.charger.list.direct_power_supply_mode` and writes only
that field through `zwrt_bsp.charger.set`, mapping true/false to enable/disable.
A missing field returns `supported:false,enabled:null,mode:null`. An unknown enum
returns `supported:true,enabled:null,mode:null`. Neither is treated as disabled,
and setting either fails with HTTP 502 before any write. Invalid parameters
return HTTP 400. Unchanged requests return `changed:false,verified:true` without
issuing a write. Changed requests return success only after bounded readback
confirms the requested enum. B20 returns an empty body on a successful write;
this is accepted only with confirmed readback. Command errors and unconfirmed writes return 502.
State is refreshed after uncertain writes as the hardware may have changed.
`verified` confirms the firmware setting, not an electrical current measurement.
No extra datad startup policy rewrites the mode; reboot persistence is whatever
the device firmware provides. The action uses the existing token authentication.
