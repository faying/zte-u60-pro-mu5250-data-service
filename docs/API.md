# zwrt-datad API

`zwrt-datad` 通过本机 HTTP 服务向上层 UFI 提供统一状态、状态事件和设备控制。

## Endpoint

- Local Base URL：`http://127.0.0.1:9460`
- LAN Base URL：`http://<device-lan-ip>:9461`
- LAN Source Filter：仅允许 `10/8`、`172.16/12`、`192.168/16`、`100.64/10`、`169.254/16` 和 `127/8`
- TLS：默认不提供；如需跨设备安全传输，应在外层补 HTTPS
- Content-Type：JSON 接口使用 `application/json; charset=utf-8`

本机端口保持免鉴权，供设备上的 UFI 和脚本使用。内网端口通过 `POST /auth/login` 或 `POST /auth/exchange` 获取临时 Token；若配置了非空静态 Token 文件，也兼容该 Token。

需要鉴权的接口接受以下请求头：

```http
Authorization: Bearer <token>
```

```http
X-Auth-Token: <token>
```

原生 `EventSource` 无法设置请求头时，也可以使用 `?access_token=<token>`。

## Routes

WebShell（`/webshell`、`/webshell/status`）、云端（`/cloud/*`）和 datad 自更新（`/ota/*`）已删除，访问返回 404。

### `POST /auth/login`

仅内网鉴权端口提供。使用 HTTP Basic 传递中兴后台用户名和密码：

```sh
curl -s -u admin:your_web_password -X POST \
  http://<device-lan-ip>:9461/auth/login
```

成功后返回有效期 12 小时的 Bearer Token；每次成功使用会刷新有效期：

```json
{"ok":true,"token_type":"Bearer","access_token":"...","expires_in":43200,"expires_at":1783500000}
```

### `POST /auth/exchange`

使用 vendor webtoken 换取 datad Token：

```sh
curl -s -X POST \
  -H 'X-Web-Token: <vendor_webtoken>' \
  -H 'X-Z-Mode: 0' \
  -H 'X-Z-Tag: zwrt-datad' \
  http://<device-lan-ip>:9461/auth/exchange
```

### `GET /version`

返回当前进程的 datad 版本，响应示例：

```json
{"name":"zwrt-datad","version":"0.9.33"}
```

版本由构建时的 `version.json` 写入二进制，不依赖设备状态和运行目录中的文件。
本机 9460 默认免鉴权；内网 9461 必须带 Bearer Token，非 GET 请求返回 405。
`/state` 和 `/events` 的 `datad` 块包含相同对象；`system.sw_version` 是设备固件版本。
旧 datad 没有此接口/字段时，应显示自身版本未知，不能用固件版本代替。

### `GET /state`

返回当前完整 JSON 快照。字段结构见 [`STATE_SCHEMA.md`](STATE_SCHEMA.md)。

### `GET /events`

建立 SSE 长连接：

1. 连接建立后立即推送当前快照
2. 只有状态内容变化时才推送下一份完整快照
3. `ts` 单独变化不会产生事件
4. 控制成功会触发立即重采样，变化后的状态通过此连接推送

```text
retry: 1000

event: state
data: {"ts":1782396733,...}

```

### `GET /v2/screen`

触屏首页信号卡和状态栏要显示的结论，由当前这份 `/state` 快照算出（`rust/src/screen.rs`）。
这些规则原来在触屏的 C 里，2026-09-26 搬到这里，屏幕只负责画；为了一条规则都不变，
`rust/tests/fixtures/screen_net_corpus.jsonl` 最初是 C 对 1700 多份快照算出的结果，测试要求逐字段相同（C 的规则随后删除，生成工具在 touch-ui tag `parity-net-v1`）；之后改规则时由 `screen/tests.rs` 的 bless 测试按 Rust 重录，patch 不动。
「信号弱」只按格数判（`sig` = 状态栏格数），RSRP 只出现在说明里；NR 载波只在现在用着 5G（SA/NSA）时列出，不在 5G 时残留的 `nr_*` 读数不算；3G/2G 上也不列 `lteca` 里残留的 LTE 载波。

不是旧接口，不冻结；靠 `v` 区分版本。`ts` 是算它用的那份快照的 `ts`。
内容只随快照变化，订阅 `/events` 的客户端在收到新快照后读一次即可。

```jsonc
{ "v": 1, "ts": 1782396733,
  "net": {
    "story": { "headline": "慢：信号弱", "hint": "RSRP -115：离基站远，靠窗通常好些", "tone": "warn",
               "cause": "weak", "rat": "5G", "sig": "弱", "sig_tone": "bad", "noise": "中", "noise_tone": "warn",
               "load": "", "limit": "—", "link": "单载波 · 带宽充足" },
    "carriers": [ { "kind": "nr", "band": 0, "label": "78", "label_short": "n78", "bw": 100, "active": true,
                    "pci": 17, "arfcn": 627264, "rsrp": "-115", "rsrq": "-11", "sinr": "5.5" } ],
    "ca_val": "单载波", "ca_sub": "↓ n78 100M   ↑ n78",
    "fine": "5G SA", "name": "中国移动", "where": "本地", "logo": "china-mobile",
    "nosvc": false, "sim_usable": true, "other": false, "roam": false, "roam_known": true,
    "bars_tier": 0, "act_n": 1, "act_bw": 100, "act_nr": 1, "act_lte": 0, "cfg": 1,
    "nr_band0": 78, "nr_mhz": 100, "have_home": true, "home_mcc": 460, "home_mnc": 0 } }
```

英文（2026-10-01 起，界面语言 L2）：原有字段一个不改，只在后面加。
- `story.state`：结论码，每个结论一个，互不相同：`nosim airplane sos nosvc nodata stall hot limit weak noise crowd only2g only3g narrow ok`（`/v2/screen` 的 `net.home` 另有写操作叠上去的 `changing revert_fail`）。`hot` = 固件在过热限速（`/state` 的 `thermal.hightemp_limit` 为 1；它读 uci `zwrt_zte_mc_tmp.cpe.hightemp_datalimit_status`，0 为 0，非 0 为 1，读不到为 null）。客户端判断状态看它，不比对中文。同一结论下的不同情况由 `hint` 区分。
- 中文里有非 ASCII 字符的文字字段，旁边多一个 `<字段>_en`：story 的 `headline hint rat link sig noise load limit`，net 的 `fine name where ca_val ca_sub mode_word`。中文为空时没有 `_en`。英文大字只写状态词（≤10 字符，如 `Slow`），原因放在 `hint_en`。
- 措辞以 manager `docs/ui-glossary.md` 为准；4 家内地运营商的 `name_en` 是 China Mobile / China Unicom / China Telecom / China Broadnet，其余运营商的广播名原样。
- 老客户端只读原字段，多出来的字段不影响它；`screen/tests.rs` 断言整个响应仍在老触屏的解析缓冲内。

### `GET /capabilities`

返回内部协议版本、支持的控制动作和事件类型。

`/ubus`、`/ubus/list`、`/ubus/call` 透传已删除（返回 404），`/capabilities` 也不再有 `discovery`、`passthrough`。设备操作只走下面的 `/control` 白名单。

### `POST /control`

执行白名单内的设备操作。接口不接受任意 Shell、任意命令名或任意 `ubus` 服务名。

```json
{
  "action": "network.set_mode",
  "params": {
    "mode": "Only_LTE"
  }
}
```

成功响应：

```json
{
  "ok": true,
  "action": "network.set_mode",
  "result": {
    "result": "success"
  }
}
```

失败响应：

```json
{
  "ok": false,
  "action": "network.set_mode",
  "error": {
    "code": "device_call_failed",
    "message": "zte_nwinfo_api.nwinfo_set_netselect failed"
  }
}
```

完整动作和参数见 [`CONTROL_API.md`](CONTROL_API.md)。

### `GET /healthz`

始终返回 `ok`。该接口只代表 HTTP 进程正在监听，不代表每个设备子模块都可用。

## Status Codes

- `200`：读取或控制成功
- `400`：请求体或参数错误
- `401`：Token 缺失或错误
- `404`：路径或控制动作不存在
- `405`：请求方法错误
- `413`：请求体超过限制
- `502`：设备侧 `ubus/uci` 调用失败
- `503`：SSE 客户端达到上限

## Command Line

- `--once`：采样一次并把 JSON 输出到标准输出
- `-i <ms>`：采样间隔，默认 `1000`
- `-b <addr>` / `--bind <addr>`：监听地址，默认 `127.0.0.1`
- `-p <port>` / `--port <port>`：监听端口，默认 `9460`
- `--lan-bind <addr>`：额外开启需要鉴权的内网监听口
- `--lan-port <port>`：内网监听端口，默认 `9461`
- `--auth-token-file <path>`：兼容静态 Token 文件

主监听地址不是回环地址时必须通过 `--auth-token-file` 启用鉴权，否则进程拒绝启动。需要同时提供本机免鉴权接口和内网接口时，应保留主监听为 `127.0.0.1`，并使用 `--lan-bind` 开启始终鉴权的内网监听。

```sh
/data/zwrt-datad/zwrt-datad -i 1000 -b 127.0.0.1 -p 9460 \
  --lan-bind 0.0.0.0 --lan-port 9461 \
  --auth-token-file /data/zwrt-datad/auth.token
```

## Integration Boundary

上层 UFI 继续提供原有 `/api/*`、`/api/goform/*` 和 `/goform/*`，负责用户鉴权、UUID、UFI 自身 OTA、插件、数据库和业务逻辑。datad 不再自带更新接口，更新靠自己编译后部署。UFI 将旧接口翻译为 datad 的内部控制动作，浏览器不应直接连接 datad。

内网读取与 SSE 示例：

```sh
curl -H 'Authorization: Bearer <token>' http://<device-lan-ip>:9461/state
curl -N 'http://<device-lan-ip>:9461/events?access_token=<token>'
```
