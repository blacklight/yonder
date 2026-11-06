#!/bin/sh

set -e

# Tag pipeline step: package the release artifacts built by
# build-binary / build-frontend-dist into a .tar.xz and publish a GitHub
# release on the blacklight/yonder mirror.
#
# Expects:
#   /release/$DRONE_TAG/yonder            — release binary
#   /release/$DRONE_TAG/frontend/dist     — built frontend
#   GH_TOKEN                              — GitHub token with contents:write
#   repo checkout                         — for config.toml.example/CHANGELOG.md

ARTIFACT_DIR="/release/$DRONE_TAG"
VERSION="${DRONE_TAG#v}"
PKG_NAME="yonder-${VERSION}-x86_64-unknown-linux-gnu"
REPO="blacklight/yonder"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

mkdir -p "$WORK/$PKG_NAME/frontend"
cp "$ARTIFACT_DIR/yonder" "$WORK/$PKG_NAME/yonder"
cp -r "$ARTIFACT_DIR/frontend/dist" "$WORK/$PKG_NAME/frontend/dist"
cp config.toml.example "$WORK/$PKG_NAME/config.toml.example"

TARBALL="$ARTIFACT_DIR/$PKG_NAME.tar.xz"
tar -cJf "$TARBALL" -C "$WORK" "$PKG_NAME"

# Release notes: the CHANGELOG section for this version (heading format
# is `## X.Y.Z`, no `v` prefix), falling back to a pointer at the file.
NOTES="$ARTIFACT_DIR/release-notes.md"
{
    echo "# $DRONE_TAG"
    echo
    awk -v ver="$VERSION" '
        $0 == "## " ver { found = 1; next }
        found && /^## / { exit }
        found { print }
    ' CHANGELOG.md
    echo
    echo "See [CHANGELOG.md](https://github.com/$REPO/blob/$DRONE_TAG/CHANGELOG.md) for details."
} > "$NOTES"

# The Forgejo -> GitHub push mirror is asynchronous; the tag may not be
# on GitHub yet. Wait for it (up to ~5 minutes) before creating the
# release — `gh release create` fails if the ref doesn't exist.
i=0
until gh api "repos/$REPO/git/ref/tags/$DRONE_TAG" >/dev/null 2>&1; do
    i=$((i + 1))
    if [ "$i" -ge 30 ]; then
        echo "error: tag $DRONE_TAG did not appear on GitHub" >&2
        exit 1
    fi
    sleep 10
done

gh release create "$DRONE_TAG" "$TARBALL" \
    --repo "$REPO" \
    --title "$DRONE_TAG" \
    --notes-file "$NOTES"
