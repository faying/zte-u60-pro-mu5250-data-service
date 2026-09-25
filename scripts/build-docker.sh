#!/bin/sh
# 在 Docker 里编 Rust 版 zwrt-datad（aarch64 musl 静态），不用装 Bootlin 工具链，
# macOS（Apple Silicon / Intel）、Linux、WSL 都能跑：
#
#   scripts/build-docker.sh [输出文件]      # 默认 ./zwrt-datad-aarch64
#
# 用 cargo-zigbuild 镜像（按 digest 固定）+ Cargo.lock（--locked）。
# 2026-09 用这个镜像从 b5e8786 编出的结果和设备上跑的 6bbf6ea6 逐字节相同。
# 编译缓存放 rust/target-zig（不进 git），依赖下载缓存在 Docker 卷 zwrt-datad-cargo。
# 打 U60 Pro（MU5250）装机包时：DATAD_BIN=<输出文件> ./onboard/build-kit.sh（manager 仓库）。
# SPDX-License-Identifier: MIT
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/zwrt-datad-aarch64}
IMAGE=${ZIGBUILD_IMAGE:-messense/cargo-zigbuild@sha256:d8313491ec5798de0633fdc1c5753761bff79967bea69076020dc78121b2cca8}
TARGET=aarch64-unknown-linux-musl

docker run --rm -v "$ROOT":/src -w /src/rust -e CARGO_TARGET_DIR=/src/rust/target-zig \
  -v zwrt-datad-cargo:/usr/local/cargo/registry "$IMAGE" sh -c "
set -e
rustup target add $TARGET >/dev/null 2>&1 || true
cargo zigbuild --locked --release --target $TARGET 2>&1 | tail -2
"
BIN=$ROOT/rust/target-zig/$TARGET/release/zwrt-datad
cp "$BIN" "$OUT"
# 装机包要求：Rust 版、没有写死的外部更新源。
# Rust 版的认法（和 manager 的 onboard/build-kit.sh 一致）：有构建标记
# ZWRT_DATAD_FORK_RUST_SELF_CONTAINED（main.rs 的 BUILD_MARKER，删掉 OTA 之后的版本），
# 或有旧的 ZWRT_DATAD_OTA_DISABLE_AUTO（删 OTA 之前的旧程序，如备份 b5e8786）。
grep -a -q -e ZWRT_DATAD_FORK_RUST_SELF_CONTAINED -e ZWRT_DATAD_OTA_DISABLE_AUTO "$OUT" ||
  { echo "不是 Rust 版 datad？" >&2; exit 1; }
if grep -a -q 'releases/latest/download' "$OUT"; then echo "带着外部更新源" >&2; exit 1; fi
ls -l "$OUT"
shasum -a 256 "$OUT" 2>/dev/null || sha256sum "$OUT"
