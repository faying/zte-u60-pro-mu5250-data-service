# zwrt-datad for the ZTE U60 Pro (MU5250)

On-device data and control service: reads `ubus`, `uci`, `sysfs` and selected logs, and serves a stable JSON state over HTTP (`GET /state`) and SSE (`GET /events`) on `127.0.0.1:9460`.
This is a fork of [33333s/zwrt-datad](https://github.com/33333s/zwrt-datad) with MU5250 fixes and a slow-data cache on `main`. **All built-in update sources are removed and auto-update is off.**

[中文](README.md) · [API](docs/API.md)

## Part of a three-repo set

| Repo | Role on the device |
|---|---|
| [manager](https://github.com/faying/zte-u60-pro-mu5250-manager) | `zte-agent` (:9090), admin web, install kit |
| [touch-ui](https://github.com/faying/zte-u60-pro-mu5250-touch-ui) | Front-panel touch UI, screen owner daemon, supervision scripts |
| **data-service** (this repo) | `zwrt-datad` (`127.0.0.1:9460`) |

Install it with the manager install kit, not on its own: see the [getting-started guide](https://github.com/faying/zte-u60-pro-mu5250-manager/blob/main/docs/GETTING-STARTED.md) (Chinese).
On the device it lives in `/data/plugins/zwrt-datad/`, supervised by procd (`/etc/init.d/zwrt-datad restart`, log `/tmp/zwrt-datad.log`).

## Build

On x86_64 Linux with the Bootlin aarch64 musl toolchain (`DATAD_MUSL_TOOLCHAIN_DIR`) and rustup: `bash scripts/build.sh` → `zwrt-datad-aarch64`.

## Docs

[API](docs/API.md) · [State schema](docs/STATE_SCHEMA.md) · [Control API](docs/CONTROL_API.md) · [Models](docs/models/)

## Credits and license

Original work by [33333s](https://github.com/33333s); contributors in [CONTRIBUTORS.md](CONTRIBUTORS.md). MIT, see [LICENSE](LICENSE). Not affiliated with ZTE.
