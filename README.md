# ZTE U60 Pro（MU5250）数据服务：zwrt-datad

`zwrt-datad` 跑在设备本机，把 `ubus`、`uci`、`sysfs` 和必要的设备日志整理成稳定的 JSON 状态，通过 HTTP 和 SSE 提供给触屏界面、脚本和其他本机服务。
本仓库是 [33333s/zwrt-datad](https://github.com/33333s/zwrt-datad) 的 fork，在 `main` 分支上加了 MU5250 的对齐修复和慢数据缓存，**并去掉了所有内置的外部更新源，自动更新默认关闭**。

[English](README_EN.md) · [API 文档](docs/API.md)

## 三个仓库一起用

| 仓库 | 设备上的角色 |
|---|---|
| [manager](https://github.com/faying/zte-u60-pro-mu5250-manager) | `zte-agent`（:9090）+ 管理网页 + 装机包 |
| [touch-ui](https://github.com/faying/zte-u60-pro-mu5250-touch-ui) | 前面板触屏界面、屏幕守护进程、进程监督与 Wi-Fi 兜底脚本 |
| **[data-service](https://github.com/faying/zte-u60-pro-mu5250-data-service)**（本仓库） | `zwrt-datad`：本机数据服务（`127.0.0.1:9460` 的 `/state` + SSE） |

```
zwrt-datad :9460 ──▶ 触屏界面 ──(eSIM 页)──▶ zte-agent :9090 ──▶ lpac ──▶ eUICC 卡
浏览器 ──▶ zte-agent :9090（API + 管理网页）
```

## 功能

- 聚合设备、CPU、内存、温度、电池，SIM、移动网络、信号、频段、流量、Wi-Fi、客户端、短信等数据
- `GET /state` 返回完整 JSON 快照，`GET /events` 用 SSE 推送变化，默认每秒一次
- 按机型模板规范化字段，`/capabilities` 报告当前能力（已适配 MU5250 / U60 Pro 等，见 [docs/models/](docs/models/)）
- `POST /control` 执行受约束的蜂窝、Wi-Fi、APN、短信、电源等控制
- 单个静态 ARM64 Rust 程序

## 快速开始

在 U60 Pro 上**不要单独装**：用 manager 仓库的装机包一起装，见 **[快速上手](https://github.com/faying/zte-u60-pro-mu5250-manager/blob/main/docs/GETTING-STARTED.md)**。
装机包把它放在 `/data/plugins/zwrt-datad/zwrt-datad`，由 procd 监督：

```sh
/etc/init.d/zwrt-datad restart            # 重启
cat /tmp/zwrt-datad.log                   # 日志
curl -fsS http://127.0.0.1:9460/healthz   # 在设备上检查
curl -fsS http://127.0.0.1:9460/state
curl -N  http://127.0.0.1:9460/events
```

装机包的启动方式带 `ZWRT_DATAD_OTA_DISABLE_AUTO=1`；程序里也没有写死的更新地址。要更新就自己编译，再用装机包 `./install.sh devui` 装上。

> **安全提示**：`POST /ubus/call` 能调用任意已注册的 ubus 方法，包括改网络、断连、重启。只对受信任的本机程序开放。

## 构建

最简单的是用 Docker（macOS / Linux / WSL 都行，不用装工具链）：

```sh
scripts/build-docker.sh   # → zwrt-datad-aarch64（静态、已 strip，镜像按 digest 固定）
```

或者在 x86_64 Linux 上，需要 Bootlin aarch64 musl 工具链（默认 `~/aarch64--musl--stable-2025.08-1/bin`，可用 `DATAD_MUSL_TOOLCHAIN_DIR` 指定）和 rustup（脚本会装 Rust 1.89.0）：

```sh
bash scripts/build.sh     # → zwrt-datad-aarch64（静态、已 strip）
```

打装机包时用 `DATAD_BIN=…/zwrt-datad-aarch64` 指定。慢变数据的缓存可用环境变量 `ZWRT_DATAD_CACHE=0` 关闭。

## 文档

- [docs/API.md](docs/API.md)：HTTP、SSE、鉴权与命令行参数
- [docs/STATE_SCHEMA.md](docs/STATE_SCHEMA.md)：状态字段约定
- [docs/CONTROL_API.md](docs/CONTROL_API.md)：控制动作与安全边界
- [docs/models/](docs/models/)：各机型模板
- [docs/RUNTIME.md](docs/RUNTIME.md)、[docs/NEIGHBOR.md](docs/NEIGHBOR.md)、[docs/CLOUD.md](docs/CLOUD.md)：上游的运行说明和可选功能（U60 Pro 装机包不使用上游安装器）

## 致谢

- [33333s](https://github.com/33333s)：`zwrt-datad` 原作者，感谢这个参考仓库（以及 [u60pro-devui](https://github.com/33333s/u60pro-devui)）。
- 上游贡献者见 [CONTRIBUTORS.md](CONTRIBUTORS.md)。
- [Jesther Silvestre](https://github.com/jesther-ai)（open-u60-pro）、Wei REN（本 fork 的 MU5250 修复和三件套整合）。

## 许可证与免责声明

[MIT](LICENSE)。社区项目，和中兴通讯没有关系，风险自负。
