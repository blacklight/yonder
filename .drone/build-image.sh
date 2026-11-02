#!/bin/sh

set -e

# Use sensible defaults for the registry and username.
REGISTRY_ENDPOINT="${REGISTRY_ENDPOINT:-quay.io}"
DOCKER_USER="${DOCKER_USER:-blacklight}"

if [ -z "$DOCKER_PASS" ]; then
    echo "Please set the DOCKER_PASS environment variable" >&2
    exit 1
fi

# Use the git tag as the image version, stripping an optional leading "v".
# Fall back to the version in Cargo.toml if DRONE_TAG is not set.
VERSION="${DRONE_TAG#v}"
if [ -z "$VERSION" ]; then
    VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
fi

IMAGE_NAME="${IMAGE_NAME:-$REGISTRY_ENDPOINT/$DOCKER_USER/yonder}"

# Log in to the container registry
echo -n "$DOCKER_PASS" | docker login "$REGISTRY_ENDPOINT" -u "$DOCKER_USER" --password-stdin

# x86_64-only for now — QEMU multiarch Rust builds are very slow on the
# shared CI host. To build arm64 later, re-add the multiarch/qemu-user-
# static setup and a `--platform` list to the buildx call below.
BUILDER=yonder
docker buildx rm "$BUILDER" 2>/dev/null || true
docker buildx create --name="$BUILDER" --driver=docker-container --use
trap 'docker buildx rm "$BUILDER" 2>/dev/null || true' EXIT

CACHE_TAG="${IMAGE_NAME}:cache"

# Build and publish the image, exporting BuildKit cache to the registry.
docker buildx build \
    -f Dockerfile \
    -t "$IMAGE_NAME:$VERSION" \
    -t "$IMAGE_NAME:latest" \
    --builder "$BUILDER" \
    --platform linux/amd64 \
    --cache-from "type=registry,ref=$CACHE_TAG" \
    --cache-to "type=registry,ref=$CACHE_TAG,mode=max" \
    --push .
