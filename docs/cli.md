# `yonder` command line

One binary runs the whole instance. With no subcommand (or `serve`)
it starts the web server; everything else is an admin/ops surface
over the same services the HTTP API uses — nothing is reimplemented
in the CLI.

## Global flags

| Flag | Effect |
|---|---|
| `--config PATH` | TOML config file (env: `YONDER_CONFIG`; default `config.toml`). |
| `--json` | Result-bearing commands print exactly one JSON value on stdout. Diagnostics and errors stay on stderr; error output is one JSON object `{"error": "...", "kind": "..."}`. Clap usage errors keep clap's own format and exit 2. |
| `--actor USERNAME` | Attribute audit-log rows to this local, active admin account instead of the default `yonder.instance` actor. |

## Commands

```
yonder serve                       # HTTP server (default)
yonder worker                      # standalone job worker + scheduler
yonder openapi                     # OpenAPI document on stdout

yonder migrate [--revert [VERSION]]
yonder migrate status              # per-migration applied/pending;
                                   # exits 1 while migrations pending

yonder config list                 # every key: toml.path ↔ YONDER_ENV
yonder config get <key> [--reveal]
yonder config set <key> <value>    # value parses as JSON, else string
yonder config unset <key>
yonder config show [--source] [--reveal]

yonder instance set-domain <DOMAIN>
yonder instance set-name <NAME>
yonder instance show

yonder run-job <kind> [--payload JSON] [--inline] [--delay SECS]

yonder user create --username U --email E
      (--password-stdin | --password-file PATH | --generate-password)
      [--role user|moderator|admin] [--skip-verification] [--locale L]
                                   # staff roles are always created
                                   # email-verified; the flag only
                                   # matters for regular users
yonder user delete <username> [--now] [--yes]
yonder user list [--role R] [--suspended] [--limit N] [--search S]
yonder user set-admin <username> [--revoke]
yonder user set-moderator <username> [--revoke]
yonder user reset-password <username> [--email always|when_enabled|never]
yonder user disable-2fa <username> [--yes]
yonder user verify-email <username>
yonder user require-reset <username>

yonder domain-block add <DOMAIN> [--severity noop|silence|suspend]
      [--reject-media] [--reject-reports] [--obfuscate]
      [--public-comment S] [--private-comment S] [--yes]
yonder domain-block remove <DOMAIN> [--yes]
yonder domain-block list [--limit N]

yonder relay add <INBOX_URL>
yonder relay authorize <ID>
yonder relay remove <ID> [--yes]
yonder relay list

yonder emoji import <PATH> [--pack-id ID] [--overwrite]
                                   # .zip, a directory under
                                   # admin.emoji_import_dir, or one image

yonder frontend install <SOURCE> [--name N] [--version V] [--ref R]
                                   # .zip, https/ssh/scp-style git URL,
                                   # or dir under install_from_path_root
yonder frontend remove <NAME> [--yes]
yonder frontend list
```

## Exit codes

| Code | Meaning |
|---|---|
| 0 | success (a declined confirmation is a successful no-op) |
| 1 | other errors; also `migrate status` "pending" signal |
| 2 | clap usage errors |
| 3 | not found |
| 4 | invalid input (`Validation`/`BadRequest`/`Conflict`), including destructive commands refusing without `--yes` on a non-TTY |
| 5 | config / migration problems |
| 6 | PostgreSQL / Redis unavailable |

## Security notes

- **Passwords never go on argv.** `user create` takes
  `--password-stdin`, `--password-file`, or `--generate-password`;
  generated passwords are printed once to stderr only. Input is
  capped at 4096 bytes.
- **`user reset-password`** mints the one-time reset URL through the
  audited `reset_link` flow and prints it to stderr only — it is a
  credential and never appears on stdout, in JSON, or in logs.
- **Sensitive config** (`auth.secret_key`, `email.smtp_password`,
  S3/API credentials, connection strings) is redacted by
  `config get`/`show` unless `--reveal`; `config set` accepts them
  with a warning (stored in the DB in plaintext) and never echoes
  them — not even in error messages or audit details.
- **`config set/unset`** writes `config_overlays` rows. The
  `server`, `database`, `redis` and `jobs` sections are read-only;
  every write validates the resulting config first and commits the
  overlay and its audit row atomically. `config unset` of a missing
  overlay exits 0 with a notice.
- **Destructive commands** (`user delete`, `disable-2fa`,
  `domain-block remove`, `domain-block add --severity suspend`,
  `relay remove`, `frontend remove`) prompt on a TTY; without
  `--yes` and a TTY they refuse before mutating (exit 4).
- **`--actor`** is audit attribution, not authorization — the CLI
  assumes host/DB-level access. Mutating commands need a real actor:
  the `yonder.instance` account (provisioned by a federated
  `serve`/`activitypub::build`) or an explicit `--actor` admin.
- **`run-job --inline`** executes production handlers in-process,
  bypassing worker retry/dead-letter accounting.
- **Running servers don't see CLI writes.** Overlay changes apply on
  the server's next reload/restart, and `domain-block` writes
  refresh only the CLI process's blocked-domain snapshot — a running
  server needs the admin API (which refreshes it live) or a restart.
