#!/bin/bash
#
# Bump the package version in Cargo.toml and Cargo.lock — the Rust
# equivalent of Songhive's [tool.bumpversion] (which lives in
# pyproject.toml and has no cargo counterpart).
#
# Uses cargo-edit's `cargo set-version`, which updates both files and
# understands semver bumps.
#
# Usage:
#   scripts/bump-version.sh patch|minor|major   # semver bump
#   scripts/bump-version.sh 0.5.0               # explicit version
#
# Then commit Cargo.toml + Cargo.lock and push a `v<version>` tag —
# the tag pipeline publishes quay.io/yonder/yonder:<version> and the
# yonder-server crate.

set -e
cd "$(dirname "$0")/.."

if ! cargo set-version --version >/dev/null 2>&1; then
    echo "cargo-edit is required: cargo install cargo-edit --locked" >&2
    exit 1
fi

case "${1:-}" in
    major|minor|patch)
        cargo set-version --bump "$1"
        ;;
    ""|-h|--help)
        echo "Usage: $0 <new-version | major|minor|patch>" >&2
        exit 1
        ;;
    *)
        cargo set-version "$1"
        ;;
esac

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
echo "version is now $version — commit Cargo.toml + Cargo.lock and tag v$version to release"
