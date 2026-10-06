# zwrt-datad Rust implementation

This directory contains the production implementation published from `main`.
The archived C and Go datad (upstream `c` branch; in this fork `base/v0.9.48`)
are no longer in the `main` tree, and Rust neither links nor executes them.

The compatibility target is the public behavior documented in `docs/API.md`,
`docs/CONTROL_API.md`, `docs/STATE_V2.md` and `docs/STATE_SCHEMA.md`; the
`/v2/state`, `/v2/events` and `/control` output is pinned by `tests/golden/`
(see `rust/tests/legacy_golden.rs`). The legacy `/state` and `/events` were
removed on 2026-10-06 (HTTP 410).

The binary includes normalized state, bounded SSE, authentication, allow-listed
controls. The upstream cloud client, signed OTA self-update,
WebShell and `/ubus` passthrough were removed in this fork, and on 2026-10-06 the
neighbor collector, fan/liquid cooling and extra SSIDs (`--webshell` and `--neighbor`
are kept as hidden no-op flags for old start scripts). CI runs the Rust unit/golden tests,
the control integration suite, the service-token and version suites.
