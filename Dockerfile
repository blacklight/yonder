# syntax=docker/dockerfile:1
ARG RUST_VERSION=1
ARG NODE_VERSION=24

###
### Frontend build → dist/
###
FROM node:${NODE_VERSION}-slim AS node-builder
WORKDIR /app/frontend
COPY frontend/package.json frontend/package-lock.json ./
# Skip lifecycle scripts: esbuild's postinstall rewrites and then
# immediately spawns its binary, which can fail on CI filesystems. The
# esbuild platform binary ships as an optional dependency, so the build
# works without the script (same as the Drone frontend-build step).
RUN npm ci --ignore-scripts
COPY frontend ./
RUN npm run build

###
### Rust build → /usr/local/bin/yonder
###
FROM rust:${RUST_VERSION}-slim AS rust-builder
# aws-lc-sys (lettre's rustls backend) compiles C code — rust:slim
# ships no toolchain. A transitive openssl-sys dep additionally needs
# pkg-config + the OpenSSL headers, and utoipa-swagger-ui's build
# script downloads the Swagger UI bundle with curl.
RUN apt-get update && apt-get install -y --no-install-recommends \
        gcc g++ cmake perl pkg-config libssl-dev curl ca-certificates && \
    rm -rf /var/lib/apt/lists/*
WORKDIR /app
# rust-toolchain.toml pins the toolchain; migrations/ and src/ are
# compile-time inputs (sqlx::migrate! embeds the schema in the binary,
# so the runtime image doesn't carry migrations/).
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY migrations ./migrations
COPY src ./src
# Tag pipelines pre-build the binary once (Drone `build-binary` step) and
# stage it at prebuilt/yonder to skip the ~40min compile here; local
# `docker build` leaves the dir empty and falls back to cargo build.
COPY prebuilt ./prebuilt
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    if [ -f /app/prebuilt/yonder ]; then \
        cp /app/prebuilt/yonder /usr/local/bin/yonder; \
    else \
        cargo build --release --locked && \
        cp /app/target/release/yonder /usr/local/bin/yonder; \
    fi

###
### Runtime image
###
# Debian release must match the rust:*-slim builder's (currently
# trixie) — a mismatch links the binary against a newer glibc than the
# runtime ships.
FROM debian:trixie-slim AS yonder
ARG USER_ID=1000
ARG GROUP_ID=1000
ENV HOME=/home/yonder
# ca-certificates: rustls-native-certs for outbound federation/SMTP
# TLS. curl: HEALTHCHECK below.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl && rm -rf /var/lib/apt/lists/* && \
    groupadd -g ${GROUP_ID} yonder && \
    useradd -u ${USER_ID} -g yonder -d /home/yonder -s /bin/bash yonder
WORKDIR /home/yonder
COPY --from=rust-builder /usr/local/bin/yonder /usr/local/bin/yonder
# Lands at the `frontends.builtin_dir` default relative to WORKDIR —
# no env override needed (compose sets it anyway for clarity).
COPY --from=node-builder /app/frontend/dist /home/yonder/frontend/dist
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh && \
    mkdir -p /data /etc/yonder /home/yonder/.local && \
    chown -R yonder:yonder /data /etc/yonder /home/yonder
VOLUME ["/data", "/etc/yonder"]
EXPOSE 3000
LABEL org.opencontainers.image.title="yonder" \
      org.opencontainers.image.description="A Mastodon-compatible ActivityPub server" \
      org.opencontainers.image.source="https://git.platypush.tech/blacklight/yonder" \
      org.opencontainers.image.licenses="AGPL-3.0-or-later"
USER yonder

# --- Optional: LaTeX formula rendering ([math]) ------------------------
# `math.enabled` shells out to a renderer command (LaTeX on stdin, PNG
# on stdout). Nothing is bundled by default — every real option is
# heavy for an opt-in feature. To enable, pick ONE recipe below, build
# a derived image (or uncomment), copy the wrapper in, and set:
#   YONDER_MATH__ENABLED=true
#   YONDER_MATH__COMMAND="/usr/local/bin/latex-math-render"
# See docs/posting.md "Renderer options" for alternatives (MathJax/
# KaTeX via Node.js, pre-made converters, Typst).
#
# Recipe A — tectonic + poppler-utils (~100 MB, recommended):
#   RUN apt-get update && apt-get install -y --no-install-recommends \
#         poppler-utils curl ca-certificates && rm -rf /var/lib/apt/lists/* \
#     && case "${TARGETARCH:-amd64}" in \
#          amd64) t=x86_64-unknown-linux-gnu ;; \
#          arm64) t=aarch64-unknown-linux-gnu ;; \
#          *) echo "unsupported arch ${TARGETARCH:-amd64}" >&2; exit 1 ;; \
#        esac \
#     && curl -fsSL "https://github.com/tectonic-typesetting/tectonic/releases/download/tectonic%400.15.0/tectonic-0.15.0-${t}.tar.gz" \
#        -o /tmp/tectonic.tar.gz \
#     && tar -xzf /tmp/tectonic.tar.gz -C /usr/local/bin --strip-components=1 \
#     && rm /tmp/tectonic.tar.gz && tectonic --version
#   # First render downloads Tectonic's TeX bundle into the cache —
#   # ensure the yonder user can write $HOME/.cache (it can).
#
# Recipe B — TeX Live via apt (~500 MB, no first-run download):
#   RUN apt-get update && apt-get install -y --no-install-recommends \
#         poppler-utils texlive-latex-base texlive-latex-recommended \
#         fonts-lmodern && rm -rf /var/lib/apt/lists/*
#
# Then, either recipe:
#   COPY scripts/latex-math-render.sh /usr/local/bin/latex-math-render
#   RUN chmod +x /usr/local/bin/latex-math-render
# ---------------------------------------------------------------------

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:${YONDER_SERVER__PORT:-3000}/healthz" || exit 1
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["yonder", "serve"]
