# Compatibility tracker

Rust is the production implementation on `main`. The previous C/Go production
line is no longer in the `main` tree (upstream `c` branch; `base/v0.9.48` in this fork)
and is not used for new releases.

| Area | Rust status | Production gate |
|---|---|---|
| CLI and static ARM64 musl build | implemented | real MU5252 `/tmp` smoke passed |
| `/healthz`, `/version`, internal snapshot (was `/state`) | MC7523 device shape parity passed; MU5252 TopFlow and all four supported-model read-only fixture matrices pass, including MC8532B UCI fallback | MU5250, MU5252 and MC8532B device golden comparisons pending |
| `/healthz` executor health (2026-10-04) | fork change: JSON `{"ok","status","exec_age_ms"}`; 503 `starting` until the first round finishes (the listener now binds before it) and 503 `stalled` after 20 s without executor progress; see `docs/API.md` | clients that only checked "it answers" must treat any HTTP reply as alive |
| change-driven `/events` SSE (removed 2026-10-06, see next row) | implemented; framing, reconnect, 16-client limit, 503 overflow and disconnect slot-release tests pass | complete |
| legacy `/state`, `/events` (2026-10-06) | removed; both answer 410 `gone` and are still counted at `/debug/legacy-hits`; `/v2/state`, `/v2/events` replace them (16-client limit now applies to `/v2/events` only) | — |
| `/capabilities` | exact 79-action legacy set (25 actions since 2026-10-06; `events` is `["snapshot","block"]`), legacy transport metadata (discovery/passthrough removed with `/ubus`), duplicate guard and formal 404 unknown-action behavior | complete |
| `/ubus`, `/ubus/list`, `/ubus/call` | removed in this fork (404) | — |
| static and dynamic authentication | static token, LAN Basic login, vendor-token exchange, 48-byte-hex sessions and sliding expiry implemented | supported-device login smoke |
| normalized device state | MC7523 structure complete; battery, NFC, authenticated/normalized SMS, thermal, bounded QoS, TopFlow aggregation/multi-WAN/cooling and slot-aware MU5252 modem state ported | MU5250 and MC8532B device value comparisons remain outside the local-only rewrite gate |
| allow-listed device controls | all 79 legacy actions implemented (57 unused ones removed on 2026-10-06, now 404 `unknown_action`); local fixtures cover fixed UBus mapping, validation, rollback, SMS RSA/AES and injection rejection | supported-device acceptance remains intentionally deferred |
| bounded neighbor QTrace parser | parser and lifecycle implemented; original parser suite passes (the `neighbor.*` actions and HTTP suite were removed on 2026-10-06) | final supported-device smoke |
| cloud config and runtime | removed in this fork (`/cloud/*` 404) | — |
| signed OTA | removed in this fork (`/ota/*` 404); updates are built and deployed by hand | — |

No Rust code may call the legacy datad binary or link the archived C/Go objects.
New releases are built exclusively from `rust/Cargo.toml` on `main`.
