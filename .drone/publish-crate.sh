#!/bin/sh

set -e

if [ -z "$CARGO_REGISTRY_TOKEN" ]; then
    echo "Please set the CARGO_REGISTRY_TOKEN environment variable" >&2
    exit 1
fi

# The package publishes as `yonder-server` — `yonder` is taken by an
# unrelated crate. The lib/bin targets stay `yonder` (see Cargo.toml).
# --no-verify: the tag pipeline already compiled this exact source in the
# build-binary step; crates.io would otherwise rebuild it a second time
# just to verify the .crate.
cargo publish --locked --no-verify --token "$CARGO_REGISTRY_TOKEN"
