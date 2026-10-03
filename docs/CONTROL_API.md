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

## Device Session

| action | params | 说明 |
|---|---|---|
| `device.login_info` | 无 | 获取设备登录 challenge |
| `device.login` | `password_hash` | 64 位 SHA-256 hex，调用 `zwrt_web.web_login` |
| `device.logout` | 无 | 清除 datad 内存中的设备会话 |
| `device.session_status` | 无 | 返回 datad 当前设备会话状态 |
| `device.change_password` | `old_hash`, `new_hash` | 修改某兴后台密码 |
| `device.reboot` | 无 | 重启设备 |
| `device.poweroff` | 无 | 关闭设备 |

UFI 自己的登录口令、HTTP 签名和浏览器会话不属于这里。

## Cellular And Radio

| action | params |
|---|---|
| `cellular.connect` | 无 |
| `cellular.disconnect` | 无 |
| `cellular.set` | `enabled?`, `roaming?`, `connect_mode?` |
| `network.set_mode` | `mode` |
| `band.set_lte` | `bands`，逗号分隔；空串表示自动 |
| `band.set_nr_sa` | `bands` |
| `band.set_nr_nsa` | `bands` |
| `cell.lock_lte` | `pci`, `earfcn` |
| `cell.lock_nr` | `pci`, `arfcn`, `band` |
| `cell.unlock_all` | 无 |
| `band.reset` | 无；原厂 `nwinfo_reset_band_cell_setting`，频段和小区锁定全部恢复默认 |
| `sim.set_slot` | `slot`，设备侧编号 `1/2` |

`cellular.set` 会先读取完整 `get_wwaniface` 对象，再覆盖调用方提供的字段，避免固件清空未指定的 PDP、配置档案等属性。

## 事务（E4 写操作层）

设计见 manager `docs/designs/write-op-layer.md`。目前只有 `network.set_mode` 走事务（`rust/src/ops/spec.rs` 的描述表），其余动作照旧。

**新客户端**在请求顶层带 `source`（`screen`、`web`、`legacy`、`guard`、`scenario`、`scheduler`、`auto`），可选 `op_id`（1–64 个字母、数字、`.`、`_`、`-`；同一个 op_id 重发只回现有状态）和 `undo`：

```json
{"action":"network.set_mode","source":"screen","op_id":"screen-42","params":{"mode":"Only_LTE"}}
```

- 写之前读当前值和 SIM（完整 ICCID + 卡槽），写之后按新鲜读数确认：配置读回 = 目标值且已注册；「应当有数据」（`get_wwaniface` 数据开关开，且不在漫游或漫游开关开）时还要数据通：`connect_status` 已连接、`zte_wan` 有 IPv4，并且一次绑定蜂窝接口（`get_wwaniface` 的 `ipv4_dev_name`）的 DNS 查询有回答（问运营商 DNS；一轮最多失败 3 次、间隔至少 5 秒，一轮都失败后同一条连接隔 30 秒再来一轮，换了连接马上重新计；通了就停）。不应当有数据时不发任何探测。退回也按同一条规则确认。成功回 200 `{"ok":true,"action":…,"result":<原厂回复>,"op":<状态>}`；写调用报错回 502，`op` 照样带上（事务按读回判断）；写之前读不到当前值回 502，没动设备。
- 同一时刻只有一个进行中的事务。别的写回 **409** `{"error":{"code":"busy"},"doing":{op_id,action,source,phase,age_ms}}`。能插队的：同一项的用户写（screen/web/legacy）和 guard（旧事务记 `superseded`，新事务的退回目标继承旧事务的）、关数据/关漫游（`cellular.set` 只含 `enabled`/`roaming` 且都为关，旧事务记 `preempted`）。
- 到点没确认：自动退回默认关（`ZWRT_DATAD_ROLLBACK=1` 才开），关着时以 `unverified/no_rollback` 结束；读回从没变成目标值以 `not_applied/ignored` 结束（不退回）。
- 状态（`op`）：`phase` 为 `accepted`、`applying`、`verifying`、`rolling_back` 或终态 `confirmed`、`unverified`、`rolled_back`、`not_applied`、`rollback_failed`、`cancelled`；`reason` 见设计稿「状态表」，另有 `sim_changed`（D32）、`reboot_loop`（D13）。`ever_matched` = 读回对上过目标值（对上过、数据一直不通，到点按没通处理，不算 `not_applied`），`data_ok` = 数据这一关过了。
- 界面用的字段（`say_zh/_en`、`steps`、`undo`、`next_zh/_en` …）、`/v2` 的 `op` 块和首页「进行中」档见 STATE_V2.md 第 12 节（E4 T13）。busy 的 `doing` 也带 `say_zh/_en`。
- `op.*`、`journal.*` 和会话、E4 新动作不进 `/capabilities`：它属于冻结的旧接口（回复要逐字节不变）。新客户端看 `/v2/screen` 有没有 `op` 判断 datad 支不支持。

| action | params | 说明 |
|---|---|---|
| `op.status` | `op_id?` | 指定的，或当前/最近结束的事务；没有为 `null` |
| `op.revert` | `op_id` | 立即退回（自动退回关着也能用）；不在等确认时回 409 `invalid_state` |
| `op.keep` | `op_id` | 保留现状，取消退回，记 `confirmed/user_keep` |
| `op.ack` | `op_id` | 「知道了」：只记账，要顶层 `source`（screen/web）；只能点最近结束的那个（`op` 块的 `last`），点过再点照样成功。别的 op_id 回 409 `invalid_state`（STATE_V2.md V2-37） |

| `journal.append` | `item` 或 `action`、`result`，可选 `reason`、`old`、`new`、`detail` 等 | 只记账（eSIM、CHILL 这类不经 `/control` 的改动由 agent 补记）；要顶层 `source`；`result` 为 `skipped` 时按下面的规则合并。不受事务锁、不进执行者队列 |
| `journal.list` | `limit?`（默认 50，最多 500） | `{"entries":[…新的在前],"owners":{项:{source,user,undo,value,op_id,ts,t}}}`；每行另带界面显示用的 `what_zh/_en`、`change_zh/_en`、`result_zh/_en`、`mark`、`source_zh/_en`、`hide`、`undo_view`（STATE_V2.md V2-41） |

**搜网会话**（E4 T7b，D17）：agent 的搜网 / 手动注册 / 回自动流程留在 agent，期间占住写锁。

| action | params | 说明 |
|---|---|---|
| `netselect.session.open` | — | 要顶层 `source`；回 `{session, max_ms, lease_ms}`。有事务或会话进行中回 409 `busy` 带 `doing` |
| `netselect.session.renew` | `session` | agent 至少每 60 秒续一次，不续就当 agent 不在了、收回；最长 7 分钟 |
| `netselect.session.close` | `session`、`result?` | 放锁；会话号不对或已结束回 409 `invalid_state` |

会话期间：影响上网的写（`cellular.set`、`network.set_mode`、锁频、锁小区、APN、卡槽、`modem.*`、`apn.set_pdp_type`）收 409，`doing.action` 是 `netselect.session`；描述表里的旧请求排队；其他写（短信、USB……）照常。会话结束（关、到点 `expired`、不续 `agent_gone`）各记一行流水账。

会话里的步骤（要顶层 `source`；`netselect.scan`、`netselect.register` 还要顶层 `session` = 当前会话号；`netselect.auto`、`cellular.redial` 带会话号算会话里的一步，不带也能做，会话结束后 guard 退回就是这样）。这几个和 `modem.*`、`apn.set_pdp_type` 先不进 `/capabilities`：

| action | params | 原厂 |
|---|---|---|
| `netselect.scan` | — | `zte_nwinfo_api nwinfo_manual_scan` |
| `netselect.register` | `mcc_mnc`（5–6 位数字）、`rat?` | `nwinfo_manual_register {m_mcc_mnc, m_rat}` |
| `netselect.auto` | — | `AT+COPS=0`（原厂没有能用的 ubus 调用） |
| `cellular.redial` | `type?`（1 IPv4、2 IPv6，不给两条都拨） | `zwrt_qcmap_cli set_qcliiface` |
| `modem.airplane` | `operate_mode`（ONLINE 以外） | `nwinfo_set_mode` |
| `modem.online` | — | `AT+CFUN=1`（`nwinfo_set_mode ONLINE` 拉不回 LPM） |
| `apn.set_pdp_type` | `ipv6`（布尔） | 拨号 APN 的 PDP 类型改成 IPv4v6 / IPv4（其他字段照原样），再拉起或断开 IPv6 那条腿 |

Wi-Fi（E4 T7b，zte-agent 的 Wi-Fi 页、热点开关、情景、家庭模式扫描用）：

| action | params | 说明 |
|---|---|---|
| `wifi.apply` | `set`（uci 路径 → 值，1–32 项）、`reload?`（默认 true）、`best_effort?` | 只认 `wireless.{main,guest}_{2g,5g}.{ssid,key,encryption,hidden,isolate,disabled,guest_active_time}`、`wireless.wifi{0,1}.{country,channel,txpowerpercent,htmode,disabled}`、`zte_mbb.wifi.{wifi_onoff,wifi6_switch}`。值和 uci 一样也照写，每个包 commit 一次，再 reload 一次（agent 靠「总是写 + reload + 自己轮询 hostapd」重试修复，不看这里的回复判断成没成）。`best_effort` 时设不上的项跳过并列在 `skipped`。回 `{committed, skipped, reloaded, reload_error}` |
| `wifi.reload` | — | 只 `zwrt_wlan reload` |

其余原厂设置（E4 T7c，zte-agent 的路由、SIM PIN、STC、省电、快速开机、充电宝、自动休眠、恢复出厂、原样发短信）：

| action | params | 说明 |
|---|---|---|
| `vendor.call` | `object`、`method`、`args?`（对象，≤ 8 KB） | 只做 `control.rs` 的 `VENDOR_CALLS` 表里的 (对象, 方法)，`args` 原样交给原厂（和 agent 以前直接调的一样）。流水账里 PIN/PUK/NCK 类字段写 `(changed)`，短信方法不记参数；恢复出厂、`system reboot` 先记 requested 并落盘再做。会话期间不挡（短信转发不能被挡）。FOTA 相关的永远不进表 |
| `sms.db_delete` | `ids`（数字和 `;`） | 原厂 `zwrt_wms_delete_sms` 删不掉 SIM 里的短信时，agent 用它在原厂的 sms.db 里直接删（固定 SQL）。不记参数 |
| `dns.doh` | `enabled`（布尔） | agent 的 DoH：写 / 删 `/tmp/dnsmasq.d/doh.conf`（转发到 127.0.0.1:5353），关的时候再去掉 `dhcp.lan_dns` 的 server/noresolv，重启 dnsmasq |

AT 只发这两条固定命令。AT 口和 zte-agent 共用，两边都拿 `ZWRT_DATAD_AT_LOCK`（默认 `/var/run/u60-at.lock`，flock）；口是 `ZWRT_DATAD_AT_PORT`，没设就按 agent 的顺序找第一个回 OK 的。等到 OK/ERROR 就停，最多 6 秒。

**流水账**（T5）：`ZWRT_DATAD_OPS_DIR` 下的 `journal.jsonl`，每行一个 JSON，带 `ts`（unix 秒）和 `t`（设备时钟的年月日时分秒，设备时钟本来就是当地时间）。会改设备的 `/control` 动作都记一行（`sms.mark_read` 不记）：事务结束时记 op_id、来源、SIM（ICCID 后 4 位/卡槽）、旧值、新值、退回目标、读回、终态和原因；不走事务的写记动作、来源（没有 source 记 `legacy`）、参数、`ok`/`failed` 和 HTTP 状态；旧请求队列的排队、被替换、丢掉也各记一行；重启、关机在执行前先记 `requested` 并等它写进闪存。密码类字段（Wi-Fi 密码、APN 用户名/密码、eSIM 激活码/确认码、PIN 等）只写 `(changed)`；ICCID、EID、IMSI、号码类字段只留后 4 位；短信动作不记参数。同一来源 + 同一项 + 同一原因的 `skipped` 只记开头一行（`skip:start`）和结束一行（`skip:end`，`count` = 一共跳过几次；原因变了，或这个来源对这一项有了别的记录时结束；计数在内存里，datad 重启时进行中的那段丢掉）。文件超过 `ZWRT_DATAD_JOURNAL_MAX_BYTES`（默认 262144）就改名为 `journal.1.jsonl`，两份合计不超过约 2 倍上限；单行最长 2 KB。`owners.json` 记每一项最后是谁写的（screen/web/legacy 的 `user` 为 true），流水账滚掉也不丢。

**旧请求**（没有 `source`）回复和以前逐字节相同。事务进行中：同一项的旧请求当覆盖写照常执行；描述表里的其他旧请求回成功（`{"result":"success"}`）并进旧请求队列，锁空出来再执行（每项只留最新、120 秒过期、关数据或重启时清空）。描述表里的动作在执行者队列满时也这样处理，不回 503；旧的关数据请求在队列满时作为内部任务马上执行。

进行中的事务落盘在 `ZWRT_DATAD_OPS_DIR`（默认 `/data/u60-ops`，空串 = 不落盘）的 `pending.json`：datad 重启接着确认；整机重启后时限重新计，第 2 次开机仍未确认或累计等待超过时限就马上退回；目录里有 `takeover` 标记（应急直写留下的）就放弃。写调用超时也回 502，但事务不判失败，只按读回判断（STATE_V2.md V2-33）。datad 的每个写都拿着跨进程写锁 `ZWRT_DATAD_WRITE_LOCK`（默认 `/var/run/u60-write.lock`），和应急直写脚本互斥。其他环境变量：`ZWRT_DATAD_OP_POLL_MS`（确认时读设备的间隔，默认 2000）、`ZWRT_DATAD_LEGACY_TTL_MS`（默认 120000）、`ZWRT_DATAD_DEADLINE_NETWORK_MODE_MS`（默认 120000）。

## WiFi, LAN And Clients

| action | params |
|---|---|
| `wifi.status` | 无，返回 `main_2g/main_5g` 配置 |
| `wireless.config` | 无参数时返回两频段国家码、信道、带宽、设备国家列表和当前监管域合法信道；写入时传 `band`，并可传 `country/channel` |
| `wifi.dual_band_status` | 无，返回双频合一能力和开关状态 |
| `wifi.set_dual_band` | `enabled`，布尔值或 `0/1` |
| `wifi.set_module` | `enabled`，`0/1` |
| `wifi.set_chip` | `chip`, `guest_enabled?` |
| `wifi.configure` | `section` 与 `ssid/encryption/key/pmf/maxassoc/hidden/isolate/enabled` 可选字段 |
| `wifi.txpower.status` | 无；返回两频段的启用状态、功率百分比、配置功率、配置上限和原厂上限 |
| `wifi.txpower.apply` | `band`=`2g`/`5g`，以及 `percent`/`limit_dbm` 至少一项；一次提交并只重载一次 WiFi |
| `wifi.txpower.set_percent` | `band`=`2g`/`5g`，`percent`=10–100（10% 步进） |
| `wifi.txpower.set_limit` | `band`=`2g`/`5g`，`limit_dbm`=1–30；同时修改 `txpower` 与 `max_power` |
| `wifi.txpower.restore_limit` | `band`=`2g`/`5g`；分别恢复为 MU5252 原厂 19/18 dBm |
| `lan.set` | `ip/netmask/dhcp_disabled/dhcp_start/dhcp_end/lease_seconds` |
| `lan.set_mtu` | `mtu` |
| `dns.set` | `primary/secondary/manual_ipv4/manual_ipv6` |
| `client.access` | 无，返回访问策略和设备列表 |
| `client.block` | `mac` |
| `client.unblock` | `mac` |
| `client.kick` | `macs`，逗号分隔 |
| `client.rename` | `mac`, `hostname` |

`wifi.configure.key` 是设备 WiFi 明文密码，只能在本机受 Token 保护的接口中传输，不应写入日志。

`wireless.config` 的国家码作用于整台无线芯片，因此写入任一频段时会同步
`wireless.wifi0.country` 与 `wireless.wifi1.country`。信道 `0` 或 `auto` 表示自动。
国家码变更后，datad 会先让厂商 `zwrt_wlan.reload` 应用监管域，再读取
`iwinfo.freqlist` 校验目标信道；因此原厂静态 `channellist` 未列出的 100-144
只有在目标国家的设备驱动实际开放时才能写入。设备重载期间会等待最长 20 秒让
`iwinfo` 恢复；校验或 reload 失败会恢复原国家码和信道。`wifi.configure` 收到的
字段与 UCI 当前值完全相同时不会重载 WiFi，避免一次页面提交重复触发无线重启。

`wifi.txpower.*` 只在 MU5252 上执行。触摸屏使用 `apply` 把百分比和上限一次提交；
`set_limit`、`restore_limit` 与 `apply.limit_dbm` 都会同时设置 radio 的 `txpower` 和
`max_power`。目标值没有变化时返回 `changed=false`，不重载 WiFi；发生变化时只提交一次
`wireless` 并重载一次 WiFi，重载失败会尝试恢复旧配置。这里返回的是配置/驱动目标，
不是天线端实测射频功率。

## APN

| action | params |
|---|---|
| `apn.list` | 无，返回模式、自动列表、手动列表和已启用 ID |
| `apn.set_mode` | `mode`，设备侧整数 |
| `apn.add` | `name`, `apn`，以及可选认证字段 |
| `apn.modify` | `profile_id`, `name`, `apn`，以及可选认证字段 |
| `apn.delete` | `profile_id` |
| `apn.enable` | `profile_id` |

认证字段为 `username/password/auth_mode/pdp_type/roaming_pdp_type`。

## USB, Sleep And NFC

| action | params |
|---|---|
| `usb.status` | 无 |
| `usb.set` | `mode/port_switch/network_protocol` |
| `sleep.status` | 无 |
| `sleep.set` | `seconds` |
| `nfc.set` | `enabled`, `flag?` |

## Traffic And QoS

| action | params |
|---|---|
| `traffic.set_limit` | `enabled`, `value?`, `type?`, `ratio?` |
| `traffic.set_clear_day` | `day` |
| `traffic.calibrate` | `value` |
| `qos.reload` | 无，重新扫描 QoS 日志 |
| `qos.clear` | 无，截断已有的 `key.log/key.log.0` 并重读；轮转文件不存在不算失败 |
| `state.refresh` | 无，立即重采样 |
| `state.set_interval` | `milliseconds`，`500..5000`；运行时切换全局采样/SSE 推送周期，不重启 datad |

## SMS

| action | params |
|---|---|
| `sms.send_raw` | `number`, `message_hex`, `sms_time`，可选 `sender` |
| `sms.delete` | `ids`，使用设备要求的分号格式 |
| `sms.mark_read` | `ids`, `tag?` |
| `sms.list_after` | `after_id?`（默认 0）, `limit?`（1～50，默认 50）；只读，返回 `{items, has_more}`，见 `STATE_V2.md` V2-30 |

`sms.send_raw` 接受已经编码的 UCS-2 hex。`sender` 为空或 `host` 时使用当前主卡；TopFlow 可选 `x75`、`v3e1`、`v3e2`，其中 V3E 通过各自内网管理接口发送；普通双卡机型可选 `sim1`、`sim2`，datad 会先用原厂 provisioning 接口激活目标卡槽并等待切换完成。主机 WMS 发送会使用 datad 已注册的厂商 AES-GCM Web 会话加密号码和正文，并轮询 `sms_cmd=4`，只有状态 3 才返回成功。文本编码、转发、黑名单和业务去重继续由 UFI 负责。

## MU5252 Aggregation And Cooling

以下动作只应在 `/state` 实际输出 `aggregation` / `cooling` 的 MU5252 模板上显示：

| action | params | 说明 |
|---|---|---|
| `aggregation.set` | `enabled`（布尔值或 `0/1`） | 开启时切到 `SMULTIWAN` 并停止 mwan3；关闭时停止 ICG、切到 `MULTIWAN` 并重启 mwan3 |
| `multiwan.interface.set` | `section` 与探测字段 | 修改已存在 interface 的启用、Ping 目标、次数、包大小、TTL、超时、间隔和上下线阈值 |
| `multiwan.member.set` | `section,metric,weight` | 修改已存在 member 的优先级与权重 |
| `multiwan.policy.set` | `section,last_resort,use_member` | 修改已存在 policy 的成员列表与无可用链路时动作 |
| `multiwan.rule.set` | `section,use_policy,sticky,logging` | 修改已存在 rule 使用的策略、会话保持与日志开关 |
| `cooling.fan.set_enabled` | `enabled` | 兼容 action 名；`true` 切到常开，`false` 切到自定义曲线 |
| `cooling.fan.set_mode` | `mode` | `automatic` 使用原厂内核三档曲线，`custom` 使用保存的 2–8 点线性曲线，`always_on` 固定 PWM 128 |
| `cooling.fan.set_curve` | `points:[{temperature,pwm},...]` | 保存并启用 datad 自定义曲线，同时退出常开；2–8 点，温度严格递增、PWM 不递减 |
| `cooling.liquid.set_enabled` | `enabled` | 兼容 action 名；实际控制“液冷常开”。`true` 固定厂商参数 `1023 60 200`，`false` 解除强制并交还 thermal 控制 |
| `cooling.liquid.set_mode` | `mode` | MU5252 液冷模式：`automatic` 交还内核 thermal；`low` 使用原厂低档幅度 60；`high` 使用原厂高档幅度 200。两档均保持频率 200，不伪造连续百分比 |

风扇/液冷配置持久化在 `/data/zwrt-datad/cooling.conf`，datad 重启时恢复。状态中的 `always_on` 表示常开，`enabled` 仅作为同值兼容别名。`automatic` 会重新启用 `sys-therm-4` 并使用设备树的 44/48/53℃、30/50/70% 三档曲线；`custom` 会禁用该 thermal zone、清零 `pwm-fan` 的锁存 state，并每秒按保存曲线线性插值写 PWM；`always_on` 采用同一用户态控制路径持续写 PWM 128。三种模式都保持风扇 `thermal_enable=1`，且 80℃ 始终强制 PWM 255。`factory_curve` 与 `custom_curve` 分别返回原厂和自定义曲线；`curve` 保留为旧消费者兼容字段。液冷自动模式恢复其 `thermal_enable`，低/高档使用原厂固定硬件参数。datad 正常退出时会把风扇和液冷 thermal 控制交还厂商驱动作为停服保护。不另装 `/etc/init.d` 或外部风扇脚本。

`multiwan.*.set` 只能修改已存在且类型匹配的 mwan3 section，不提供任意 UCI 路径写入。datad 会先校验所有 section、数值范围、IP 地址和引用关系再提交；`use_member` 与 `track_ip` 以受限列表替换。`MULTIWAN` 模式保存后重启 mwan3 并返回 `applied=true`，`SMULTIWAN` 模式只保存并返回 `applied=false`。

示例：

```json
{"action":"cooling.fan.set_curve","params":{"points":[
  {"temperature":40,"pwm":0},
  {"temperature":50,"pwm":76},
  {"temperature":60,"pwm":128},
  {"temperature":70,"pwm":255}
]}}
```

## Safety

- 所有动作必须存在于编译期白名单。
- 服务名和方法名不能由请求指定。
- `ubus/uci` 使用 `fork/exec` 参数数组执行，不经过 Shell。
- WiFi、APN 和密码字段不得写入运行日志。
- 重启、关机和密码修改应由 UFI 再做用户确认。
- 切换 `SMULTIWAN` 会重配 WAN，远程设备可能短暂断线；UFI 应明确提示用户。


### Topflow advanced wireless

`wifi.advanced.status` returns two radio entries and the configured SSIDs, including
live interface names, band, readiness, driver-reported power and PSM. Passwords
are omitted. Power readback is a driver/firmware value, not an RF measurement.

- `wifi.txpower.set_dbm`: `{band:"2g"|"5g", dbm:1..30}` stores a 1 dBm-step policy;
  the current channel's reported limit is enforced when available. `{band,mode:"oem"}`
  removes it and reapplies the OEM configured power. The legacy percentage setter
  rejects changes while a dBm policy is active.
- `wifi.psm.set`: `{section,mode:"on"|"off"|"default"}` persists an SSID-specific
  policy. `default` releases ownership and retains the current driver state until
  the next driver reset. Inactive interfaces receive saved policies when ready.
- `wifi.interface.configure`: `{section,ssid?,encryption?,key?,enabled?,hidden?,isolate?}`
  edits a stock main/guest interface. A blank key retains the existing password.
  `enabled`, `hidden` and `isolate` are 0 or 1. OEM changes can restart Wi-Fi;
  callers must poll readiness instead of treating save success as an active AP.
- `wifi.interface.create`: `{band,ssid,encryption,key,enabled?,hidden?,isolate?}`
  allocates one of two additional SSID slots. `wifi.interface.configure` also
  accepts those returned section IDs, and an optional `band` for them.
- `wifi.interface.delete`: `{section}` deletes only an additional SSID.

Supported additional-SSID encryption values are `none`, `psk2+ccmp`, `sae-mixed`
and `sae`. SSIDs contain 1-32 UTF-8 bytes; encrypted passwords contain 8-63 bytes.
Additional SSIDs bridge to the existing LAN; AP isolation does not provide a
separate guest subnet or block access to other LAN devices.

The OEM QCMAP loader accepts its four predefined sections. Additional SSIDs live
in the separate UCI package `datad_wifi` and use independently managed hostapd
processes in `/data/zwrt-datad/wifi/`, with private config files and reserved
interfaces wlan4/wlan5. Creation waits for an active stock AP on the same band.
The runtime restores these APs after a radio restart without installing hotplug
scripts or modifying the OEM loader. Ownership checks use PID command lines plus
boot ID and interface index before cleanup. PSM policies apply after readiness, once per interface generation or explicit edit.
Power is reapplied once after an atomic wireless config revision has settled,
because the OEM post-DFS workflow commits configuration before resetting power.
This does not reassert PSM, and it does not continuously fight arbitrary live
changes. Additional APs inherit the OEM computed radio power when no dBm override
is selected.

## Neighbor collection

| action | params | Result |
|---|---|---|
| `neighbor.status` | none | Current cached neighbor block |
| `neighbor.set` | `enabled`: boolean or 0/1 | Process-scoped enablement and current lifecycle state |

Collection is off by default and enabling lasts only until datad restarts. Older
saved `enabled:true` values are reset to false during startup. Disabling waits for
the owned collector to stop and removes its capture logs before returning success.
A successful enable request confirms startup, not compatible firmware reports; inspect `status`, `reason`
and `cells`. See [NEIGHBOR.md](NEIGHBOR.md) for the supported signature limits.

## Charger direct supply

| action | params | Result |
|---|---|---|
| `power.direct_supply.status` | none | `supported`, `enabled`, `mode` |
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
the device firmware provides. Both actions use the existing token authentication.
