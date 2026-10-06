# ZTE U60 Pro (MU5250) data service: zwrt-datad

**English** · [中文](README.zh-CN.md) · [API](docs/API.md)

`zwrt-datad` runs on the device itself. It turns `ubus`, `uci`, `sysfs` and the device logs it needs into a stable JSON state, served over HTTP and SSE to the touch UI, scripts and other local services.
It started from [33333s/zwrt-datad](https://github.com/33333s/zwrt-datad) and is now maintained independently, for the U60 Pro (MU5250): it adds MU5250 alignment fixes, a slow-data cache and a single ubus reader with a write-op layer, **and removes the original's self-updater (OTA), cloud client, WebShell and `/ubus` passthrough; the binary does not contact any Internet address**.

The documents linked under `docs/` are in Chinese.

## Using the three repos together

| Repo | Role on the device |
|---|---|
| [manager](https://github.com/faying/zte-u60-pro-mu5250-manager) | `zte-agent` (:9090) + admin web + install kit |
| [touch-ui](https://github.com/faying/zte-u60-pro-mu5250-touch-ui) | Front-panel touch UI, screen daemon, process supervision and Wi-Fi fallback scripts |
| **[data-service](https://github.com/faying/zte-u60-pro-mu5250-data-service)** (this repo) | `zwrt-datad`: local data service (`/v2/state` + SSE on `127.0.0.1:9460`) |

```
zwrt-datad :9460 ──▶ touch UI ──(eSIM page)──▶ zte-agent :9090 ──▶ lpac ──▶ eUICC card
browser ──▶ zte-agent :9090 (API + admin web)
```

## Features

- Aggregates device, CPU, memory, temperature, battery, SIM, mobile network, signal, bands, traffic, Wi-Fi, clients, SMS and other data
- `GET /v2/state` returns the current state blocks; `GET /v2/events` sends a snapshot on connect, then only the blocks that changed, over SSE (the old `/state` and `/events` were removed on 2026-10-06 and return 410)
- Normalizes fields per model template; `/capabilities` reports the current capabilities (MU5250 / U60 Pro and others supported, see [docs/models/](docs/models/))
- `POST /control` performs constrained cellular, Wi-Fi, APN, SMS, power and other controls
- A single static ARM64 Rust binary

## Quick start

On the U60 Pro, **don't install it on its own**: install it together with the manager repo's install kit, see **[Getting started](https://github.com/faying/zte-u60-pro-mu5250-manager/blob/main/docs/GETTING-STARTED.md)**.
The install kit puts it at `/data/plugins/zwrt-datad/zwrt-datad`, supervised by procd:

```sh
/etc/init.d/zwrt-datad restart            # restart
cat /tmp/zwrt-datad.log                   # log
curl -fsS http://127.0.0.1:9460/healthz   # check on the device
curl -fsS http://127.0.0.1:9460/v2/state
curl -N  http://127.0.0.1:9460/v2/events
```

The binary has no self-updater (the original's OTA, cloud client, WebShell and `/ubus` passthrough are all removed) and no hard-coded Internet addresses. To update, build it yourself and install it with the install kit's `./install.sh devui`.

## Build

The simplest way is Docker (macOS / Linux / WSL all work, no toolchain to install):

```sh
scripts/build-docker.sh   # → zwrt-datad-aarch64 (static, stripped, image pinned by digest)
```

Or on x86_64 Linux, you need the Bootlin aarch64 musl toolchain (default `~/aarch64--musl--stable-2025.08-1/bin`, override with `DATAD_MUSL_TOOLCHAIN_DIR`) and rustup (the script installs Rust 1.89.0):

```sh
bash scripts/build.sh     # → zwrt-datad-aarch64 (static, stripped)
```

When building the install kit, point to it with `DATAD_BIN=…/zwrt-datad-aarch64`. The cache for slow-changing data can be turned off with the environment variable `ZWRT_DATAD_CACHE=0`.

## Docs

- [docs/API.md](docs/API.md): HTTP, SSE, authentication and command-line options
- [docs/STATE_V2.md](docs/STATE_V2.md): `/v2` state stream (blocks, events, collection rules)
- [docs/STATE_SCHEMA.md](docs/STATE_SCHEMA.md): fields of the internal snapshot the `/v2` blocks are cut from
- [docs/CONTROL_API.md](docs/CONTROL_API.md): control actions and safety boundaries
- [docs/models/](docs/models/): model templates
- [docs/RUNTIME.md](docs/RUNTIME.md): runtime notes from the original (the U60 Pro install kit uses its own startup method)

## Credits

- [33333s](https://github.com/33333s): original author of `zwrt-datad`; thanks for this reference repo (and [u60pro-devui](https://github.com/33333s/u60pro-devui)).
- Contributors to the original are listed in [CONTRIBUTORS.md](CONTRIBUTORS.md).
- [Jesther Silvestre](https://github.com/jesther-ai) (open-u60-pro), Wei REN (MU5250 fixes and the three-repo integration).

## License and disclaimer

[MIT](LICENSE). A community project, not affiliated with ZTE Corporation; use at your own risk.
