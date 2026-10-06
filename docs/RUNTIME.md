# 运行与日志

`zwrt-datad` 读取设备 `ubus/uci/sysfs` 与 QoS 日志，并在本机 `127.0.0.1:9460` 提供 HTTP/SSE 和白名单控制接口。上游的云端、自更新（OTA）、WebShell 和 `/ubus` 透传已删除。

## 启动方式

U60 Pro（MU5250）上由 procd 管：`/etc/init.d/zwrt-datad`（脚本在 touch-ui 仓库的 `scripts/zwrt-datad.init`），
程序在 `/data/plugins/zwrt-datad/`，开机只走 `/etc/rc.local` 里的 `/etc/init.d/zwrt-datad start`。init 脚本负责生成 Token、
带上运行参数：

```sh
/data/plugins/zwrt-datad/zwrt-datad -i 1000 --lan-bind 0.0.0.0 --lan-port 9461 \
  --auth-token-file /data/plugins/zwrt-datad/auth.token
```

`--auth-token-file` 现在是有效运行参数。文件首行去除首尾空白后作为 Bearer Token；指定了该参数但文件不存在或为空时，进程拒绝启动。回环主监听（9460）不要求 Token；`--lan-bind` 的 LAN 监听上，`/healthz` 保持公开，其余数据和控制接口要求：

主监听若配置为非回环地址但未启用鉴权，datad 会拒绝启动，避免误把控制接口暴露到网络。对外提供内网访问时使用 `--lan-bind`，该监听始终要求鉴权。

```http
Authorization: Bearer <token>
```

也兼容仅供本机服务间调用的 `X-Auth-Token` 请求头。不要把 Token 写入前端静态文件。

Token 文件由 init 脚本首次启动时从 `/dev/urandom` 生成，权限 `0600`。`--webshell` 是已删除的 WebShell 的旧开关，
只为不让还带着它的旧启动脚本起不来，保留为隐藏的空开关，不起任何作用。

不要把长期、无轮转的输出重定向到 `/tmp/*.log`。在常见 OpenWrt 设备中，`/tmp` 位于 tmpfs；如果某个扩展构建或诊断后端输出高频调试信息，日志文件会直接占用 RAM，表现为“可用内存持续下降”，并不等同于进程 RSS 泄漏。

若确实需要保留诊断日志，应使用具有容量上限和轮转策略的持久化目录；诊断结束后及时停用高频输出并清理旧文件。

## 运行检查

```sh
curl -fsS http://127.0.0.1:9460/healthz
curl -fsS http://127.0.0.1:9460/state
```

`/healthz` 返回 200（`{"ok":true,"status":"ok","exec_age_ms":…}`）表示第一轮采集已完成、采集执行者在前进；
刚启动（第一轮还没采完）回 503 `starting`，执行者超过 20 秒没前进回 503 `stalled`（见 [`API.md`](API.md)）。
datad 先监听再采第一轮，所以 ubusd 不回时端口照样能连上、`/healthz` 照样回答。`/state` 用于检查最新聚合快照（启动中先等第一轮，最多 10 秒，还没好回 503 `starting`）。
