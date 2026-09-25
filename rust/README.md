# zwrt-datad Rust implementation

This directory contains the production implementation published from `main`.
The archived C and Go datad (upstream `c` branch; in this fork `base/v0.9.48`)
are no longer in the `main` tree, and Rust neither links nor executes them.

The compatibility target is the public behavior documented in `docs/API.md`,
`docs/CONTROL_API.md` and `docs/STATE_SCHEMA.md`; the legacy `/state` and
`/events` output is pinned by `tests/golden/` (see `rust/tests/legacy_golden.rs`).

The binary includes normalized state, bounded SSE, authentication, allow-listed
controls and neighbor parsing. The upstream cloud client, signed OTA self-update,
WebShell and `/ubus` passthrough were removed in this fork (`--webshell` is kept
as a hidden no-op flag for old start scripts). CI runs the Rust unit/golden tests,
the control integration suite, the service-token, version and neighbor suites.
