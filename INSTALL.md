# Installation

<!-- toc -->

- [🐳 Docker](#docker)
- [🦀 Cargo](#cargo)
- [📦 Direct binary download](#direct-binary-download)
- [🚧 Building from source](#building-from-source)

<!-- tocstop -->

## 🐳 Docker

`docker-compose.yml` runs the full stack — `yonder` (HTTP), `worker`
(job queues), `postgres`, `redis` and an `nginx` reverse proxy that
fronts everything on `YONDER_HTTP_PORT` (default 80). The image is
published at `quay.io/yonder/yonder`; uncomment the `build:` blocks to
build it locally instead.

```bash
cp config.toml.example config.toml   # REQUIRED before first `up` —
                                     # the bind mount fails loudly
                                     # without it
cp .env.example .env                 # PUID/PGID, domain, secrets
docker compose up -d
```

Layout and conventions:

- **Volumes** — `./volumes/{data,db,redis}` bind-mounted into the
  containers (`/data` holds media, installed frontends and the
  generated `secret_key`). A root `setup` container creates and chowns
  them to `PUID`/`PGID` before the stack starts; the services run
  rootless as that UID/GID.
- **Config** — `./config.toml` mounts read-only at
  `/etc/yonder/config.toml` (`create_host_path: false` — create it
  first). `.env` reaches the containers via `env_file`, so any
  `YONDER_<SECTION>__<KEY>` override or secret works there.
- **Entrypoint** — generates `YONDER_AUTH__SECRET_KEY` into
  `/data/secret_key` when unset (race-free across app and worker),
  waits for Redis/Postgres and runs `yonder migrate` before `serve`/
  `worker`. Non-daemon subcommands (`user create`, `webpush …`,
  `openapi`, `config`, `run-job …`) skip the waits:
  `docker compose run --rm yonder user create …`.
- **nginx** (`docker/nginx.conf`) — proxies the app, upgrades
  `/api/v1/streaming` WebSockets, serves `local`-backend media straight
  from a read-only mount, and edge-caches anonymous ActivityPub
  dereferences (`yonder_fedoc` zone, single-flighted). The app bounds
  that cache itself: public documents get `X-Accel-Expires` + a public
  `Cache-Control`, private/credentialed answers get `X-Accel-Expires:
  0` + `no-store`, and signed fetches bypass the cache entirely.

## 🦀 Cargo

The server is published on crates.io as `yonder-server` (`yonder` is
taken by an unrelated project; the binary is still `yonder`):

```bash
cargo install yonder-server --locked

# PostgreSQL and Redis are required — then configure and run
cp config.toml.example config.toml
yonder serve
```

## 📦 Direct binary download

```bash
VERSION="1.0.3"  # IMPORTANT: Set to the version you want
wget -O yonder.tar.xz https://github.com/blacklight/yonder/releases/download/v${VERSION}/yonder-${VERSION}-x86_64-unknown-linux-gnu.tar.xz
tar -xf yonder.tar.xz && cd yonder-${VERSION}-x86_64-unknown-linux-gnu
cp config.toml.example config.toml
./yonder serve
```

## 🚧 Building from source

```bash
git clone https://github.com/blacklight/yonder
cd yonder
cargo build --release
cp config.toml.example config.toml
./target/release/yonder serve
```
