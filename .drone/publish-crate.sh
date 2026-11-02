#!/bin/sh

set -e

if [ -z "$CARGO_REGISTRY_TOKEN" ]; then
    echo "Please set the CARGO_REGISTRY_TOKEN environment variable" >&2
    exit 1
fi

# The package publishes as `yonder-server` — `yonder` is taken by an
# unrelated crate. The lib/bin targets stay `yonder` (see Cargo.toml).
cargo publish --locked --token "$CARGO_REGISTRY_TOKEN"
