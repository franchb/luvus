#!/usr/bin/env bash
# Measure the VT engines against each other through Luvus's own code paths.
#
# Runs the benchmark once per engine, in separate processes so the two never
# share an allocator, and prints both reports. See src/terminal/vt/bench.rs
# for what is measured and what the numbers mean.
#
# The shitty engine links a C++ library that is not on crates.io, so this
# needs a packaged shitty release tree:
#
#   cd /path/to/shitty && ./build tgz
#   mkdir -p ~/.local/share/shitty-vt
#   tar xzf .build/shitty_vt.tgz -C ~/.local/share/shitty-vt
#   prefix=~/.local/share/shitty-vt/shitty_vt-<version>
#   export PKG_CONFIG_PATH="$prefix/lib/pkgconfig" LD_LIBRARY_PATH="$prefix/lib"
#
# Numbers are comparable between engines on one machine, and between commits
# on one machine. They are not comparable between machines.
#
# Usage: scripts/bench-engines.sh [engine ...]   (default: alacritty shitty)
set -euo pipefail

cd "$(dirname "$0")/.."

if [ "$#" -gt 0 ]; then
    engines=("$@")
else
    engines=(alacritty shitty)
fi

for engine in "${engines[@]}"; do
    LUVUS_VT_ENGINE="$engine" cargo test \
        --release \
        --features shitty-engine \
        --bin luvus \
        bench_engines \
        -- --ignored --nocapture 2>/dev/null |
        sed -n '/^engine /,/^capture /p'
done
