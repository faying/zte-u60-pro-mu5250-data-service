# zwrt-datad layers

Since October 2026 the code is split into four layers. Data flows one way:
IO reads → projection computes → HTTP serves; writes come in over HTTP and go
out through the write layer to IO.

| Layer | What it does | Files |
|---|---|---|
| **Projection** (pure) | Raw replies in, `/state`-shaped fields, `/v2` blocks and `/v2/screen` out. No requests, files, env, clock or global state; `project.rs` has a test that enforces it. | `project/snapshot.rs`, `project/screen.rs` |
| **IO** | Every read of the device: ubus (one executor, one request in flight), uci (own parser, falls back to `uci show`), sysfs/procfs samplers, the bearer log, SMS decryption, AT commands. Also the block bookkeeping (`block.rs`: revision, stale, seq) and the internal watchdog. | `state.rs` (`collect` = `read_inputs` → `project` → `mu5252_extras` → record blocks), `executor.rs`, `ubus/`, `uci.rs`, `qos.rs`, `sms.rs`, `at.rs`, `command.rs`, `wifi.rs`, `cell_window.rs`, `block.rs`, `watchdog.rs` |
| **HTTP** | axum routes and middleware, auth, connection tuning, SSE framing, app start and shutdown. For `POST /control` only parsing and checking the body. | `server.rs`, `conn.rs`, `auth.rs`, `v2.rs`, `legacy_hits.rs`, `model.rs` |
| **Write** | What happens to a `/control` request after parsing: start-up wait, E4 transactions, legacy queue, the executor job, the cross-process write lock, the journal, re-reading the affected blocks; then the actions themselves. | `server/write.rs`, `control.rs`, `ops/` |

Some pure helpers still sit in IO files: `qos::select` and the log parsers,
`uci::parse`, `block.rs` policies, `sms::block_data`. The MU5252 multi-modem part
(`state::mu5252_extras`) reads per slot and per interface while it builds its
output, so it stays in the IO layer as it was.

`write.rs` still returns axum `Response`s; separating write results from HTTP
status codes is left for when the write path moves (u60 platform Phase 3).

Checks that pin the behaviour while moving code: the `/v2` golden files
(`tests/golden/`), `/control` golden and contract tests, the screen corpus, and
for the full snapshot `zwrt-datad --once` against the mock device.
