#!/bin/sh

export RUSTFLAGS="-C link-arg=-fuse-ld=mold -C target-cpu=skylake"
cargo build --release -p signal-gateway-bin
