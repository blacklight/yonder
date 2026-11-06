#!/bin/bash
#
# Bump the package version in Cargo.toml and Cargo.lock — the Rust
# equivalent of Songhive's [tool.bumpversion] (which lives in
# pyproject.toml and has no cargo counterpart).
#
# Uses cargo-edit's `cargo set-version`, which updates both files and
# understands semver bumps. The first "Unreleased" heading in
# CHANGELOG.md is renamed to the new version. Afterwards it commits
# the bumped files and creates a `v<version>` tag, like bumpversion's
# commit/tag options.
#
# Usage:
#   scripts/bump-version.sh patch|minor|major   # semver bump
#   scripts/bump-version.sh 0.5.0               # explicit version
#
# Push the commit and the tag to publish — the tag pipeline builds
# quay.io/yonder/yonder:<version> and the yonder-server crate.

set -e
cd "$(dirname "$0")/.."

if ! cargo set-version --version >/dev/null 2>&1; then
    echo "cargo-edit is required: cargo install cargo-edit --locked" >&2
    exit 1
fi

old_version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

# Fail before touching anything if there's no Unreleased section to
# stamp — bumpversion errors the same way on a missing match.
if ! grep -qE '^[[:space:]]*#+[[:space:]]+Unreleased[[:space:]]*$' CHANGELOG.md; then
    echo "CHANGELOG.md has no 'Unreleased' heading to bump" >&2
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
tag="v$version"

if [ "$version" = "$old_version" ]; then
    echo "version is already $version — nothing to commit or tag" >&2
    exit 1
fi

if git rev-parse --verify --quiet "refs/tags/$tag" >/dev/null; then
    echo "tag $tag already exists — refusing to reuse it" >&2
    exit 1
fi

# Rename the first Unreleased heading to the new version, preserving
# its heading level (## Unreleased -> ## 0.2.0).
sed -i -E "0,/^([[:space:]]*#+)[[:space:]]+Unreleased[[:space:]]*$/s//\1 $version/" CHANGELOG.md

# Commit only the bumped files so unrelated staged work stays staged.
git commit -m "Bump version: $old_version → $version" \
    -m "Bumped files:
- Cargo.toml: $version
- Cargo.lock: $version
- CHANGELOG.md: ## $version" \
    -- Cargo.toml Cargo.lock CHANGELOG.md

git tag "$tag"

echo "version is now $version — committed and tagged $tag"
echo "push with: git push && git push origin $tag"
