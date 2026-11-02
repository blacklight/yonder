#!/bin/bash
set -e

DATA_DIR=/data
SECRET_FILE="$DATA_DIR/secret_key"

# Yonder subcommands (`src/cli/mod.rs` Command enum). When the first
# non-flag argument is one of these, prepend the binary name so
# compose `command: worker` works like `yonder worker`.
SUBCOMMANDS="serve worker migrate config instance run-job user domain-block relay emoji frontend webpush openapi"

# Generate and persist an auth secret if the caller did not provide one.
# The app and worker containers share the /data volume, so an
# unprotected check-then-create would race and could leave an empty
# secret_key file. Use a file lock and an atomic temp-file move instead.
_ensure_secret() {
    if [ -s "$SECRET_FILE" ]; then
        return 0
    fi

    mkdir -p "$DATA_DIR"
    (
        [ -e "${SECRET_FILE}.lock" ] || : > "${SECRET_FILE}.lock"
        flock -x 9
        # Re-check after acquiring the lock; another container may have created it.
        if [ -s "$SECRET_FILE" ]; then
            exit 0
        fi
        _tmp=$(mktemp "${SECRET_FILE}.XXXXXX")
        # No openssl/python in the image — urandom + base64 it is.
        head -c 48 /dev/urandom | base64 > "$_tmp"
        chmod 600 "$_tmp"
        mv -f "$_tmp" "$SECRET_FILE"
    ) 9> "${SECRET_FILE}.lock"
}

_ensure_secret

if [ -z "${YONDER_AUTH__SECRET_KEY:-}" ]; then
    YONDER_AUTH__SECRET_KEY="$(cat "$SECRET_FILE")"
    export YONDER_AUTH__SECRET_KEY
fi

# Extract host and port from YONDER_REDIS__URL (default redis:6379) for
# the TCP wait loop below.
_redis_hostport() {
    local url="${YONDER_REDIS__URL:-redis://redis:6379/0}"
    url="${url#*://}"   # strip scheme
    url="${url##*@}"    # strip userinfo
    url="${url%%/*}"    # strip path / db number
    local host="${url%%:*}"
    local port="${url##*:}"
    [ "$port" = "$host" ] && port=6379
    echo "${host:-redis} ${port:-6379}"
}

wait_for_redis() {
    read -r host port <<< "$(_redis_hostport)"
    for _ in $(seq 30); do
        if (exec 3<>"/dev/tcp/$host/$port") 2>/dev/null; then
            return 0
        fi
        sleep 1
    done
    echo "Redis is not reachable" >&2
    exit 1
}

# The first non-flag argument decides whether this is a daemon
# invocation. `--config`/`--actor` consume a value; other flags
# (`--json`) don't.
subcommand_of() {
    local skip_next=0
    for arg in "$@"; do
        if [ "$skip_next" -eq 1 ]; then
            skip_next=0
            continue
        fi
        case "$arg" in
            --config|--actor|--config=*|--actor=*) [ "$arg" = "${arg%%=*}" ] && skip_next=1 ;;
            -*) ;;
            *) echo "$arg"; return 0 ;;
        esac
    done
}

CMD="$(subcommand_of "$@")"

if [ -z "$CMD" ]; then
    # `--help`/`--version` (with or without `--config`/`--actor`)
    # hand straight to the CLI — no subcommand to normalize, no
    # datastore to wait on.
    for arg in "$@"; do
        case "$arg" in
            --help|-h|--version|-V) exec yonder "$@" ;;
        esac
    done
    # No subcommand — the CLI's default is `serve`, a daemon command.
    # Normalize bare invocations while preserving any global flags
    # (`yonder --config /etc/yonder/config.toml` still means serve).
    if [ $# -eq 0 ]; then
        set -- yonder serve
    else
        set -- yonder "$@"
    fi
    CMD=serve
fi

for sub in $SUBCOMMANDS; do
    if [ "$sub" = "$CMD" ]; then
        set -- yonder "$@"
        break
    fi
done

# Daemon commands wait on the datastore and run migrations before
# exec. `yonder migrate` is idempotent and advisory-locked, and the
# call doubles as the Postgres wait. Non-daemon subcommands
# (`user create`, `openapi`, `webpush generate-keys`, `config`, …)
# skip both waits so they work without live services.
case "$CMD" in
    serve|worker|migrate|"")
        wait_for_redis
        db_ready=0
        for _ in $(seq 30); do
            if yonder migrate; then
                db_ready=1
                break
            fi
            sleep 2
        done
        if [ "$db_ready" -eq 0 ]; then
            echo "Database is not reachable or migrations failed" >&2
            exit 1
        fi
        ;;
esac

exec "$@"
