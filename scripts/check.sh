#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
luac -p zippity.yazi/main.lua
luac -p zippity.yazi/actions.lua
lua tests/lua_contract.lua
python3 tests/formats.py
python3 tests/writes.py
python3 tests/reader.py
python3 tests/cancel_copy.py
python3 tests/salvage.py
python3 tests/yazi_smoke.py
python3 tests/preview.py
python3 tests/compressed.py
python3 tests/containers.py
python3 tests/links.py
python3 tests/finder.py
python3 tests/kept_listing.py
cargo build --release --locked
python3 tests/bench.py
