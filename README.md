# wardn

Open source org & access layer for [`Mynd`](https://github.com/oxhive/mynd),
a memory MCP server for AI coding agents.

Wardn gives Mynd organization structure: creating an org, inviting and
removing members, and assigning roles that control who can read and write
an org's shared (Layer 3) memory. It is not a billing system, not a
multi-tenant SaaS platform, and not a personal memory sync server. See
`WARDN_SPECIFICATION.md` for the full design and the "What Wardn Is NOT"
section in particular.

```
Mynd solves:   "Claude remembers you across sessions"
Wardn solves:  "your team can share and control access to memory,
                together, under your own roof if you want"
```

**Status:** implements the full CLI surface (org/member/role/key
management), a local libSQL-backed store, `wardn serve` (the narrow HTTP
authorization API Mynd calls before touching org-layer memory), and
toggleable observability (Prometheus metrics, OTel tracing, Loki logs; see
below). No billing, no multi-org routing, no GUI: that's all deliberately
out of scope for this repository (see `WARDN_SPECIFICATION.md`).

## Quickstart

```sh
cargo install --path .
wardn init --name "Acme Corp"
```

This creates a local libSQL database (default: `~/.local/share/wardn/org.db`,
override with `--db <path>` or `$WARDN_DB_PATH`) and the org record.

```sh
wardn org invite alice@example.com --role admin
wardn org members list
wardn keys create --member <member_id> --name laptop
# -> prints the full API key exactly once; store it somewhere safe
```

Start the authorization service Mynd instances call:

```sh
wardn serve                          # binds 127.0.0.1:7787 by default
                                      # override with --listen or $WARDN_LISTEN_ADDR
```

```sh
curl http://127.0.0.1:7787/healthz   # -> ok

curl -X POST http://127.0.0.1:7787/v1/authorize \
  -H 'content-type: application/json' \
  -d '{"api_key":"wd_...","action":"write"}'
# -> {"allowed":true,"member_id":"...","role":"admin"}
```

## CLI surface

```sh
wardn init                          # first-run setup, creates local
                                     # org.db, prompts for org name
wardn org create --name <name>
wardn org rename --name <name>
wardn org delete                    # requires confirmation
wardn org invite <email> --role <admin|member|read_only>
wardn org members list
wardn org members remove <member_id>
wardn org role set <member_id> <role>
wardn keys create --member <member_id> --name <label>
wardn keys revoke <key_id>
wardn keys list
wardn status                        # org name, member count, storage path,
                                     # whether wardn serve is reachable
wardn serve                         # start the HTTP service Mynd
                                     # authorizes against
wardn service install               # run `wardn serve` as a background
                                     # OS service (see "Running as a
                                     # service" below)
wardn service status                # is the service installed, enabled,
                                     # running, and actually reachable
wardn service uninstall             # stop it and remove the service
                                     # definition
```

Every operation a self-hosted team needs is available via this CLI. A
paid, closed-source GUI (`console`) exists as a separate, optional product
for people who'd rather click than type, but it holds no capability the
CLI lacks. See Decision 5 in `WARDN_SPECIFICATION.md`.

## Roles

```
admin:      full control: invite/remove members, change roles,
              read + write org-layer memory, rename/delete the org
member:     read + write org-layer memory, cannot manage membership
read_only:  read org-layer memory only, cannot write, cannot
              manage membership
```

## Observability (optional, toggleable)

`wardn serve` can export Prometheus metrics, OTel traces, and Loki logs.
None of it is required, and each piece toggles independently:

```
Compile-time toggle:  the "observability" feature, on by default.
                       `cargo build --no-default-features` drops OTel,
                       Prometheus, and Loki as dependencies entirely, for
                       the smallest possible binary (e.g. a Raspberry Pi).

Runtime toggle:        with the feature compiled in, each exporter still
                       stays off until its env var is set:
                         WARDN_METRICS_TOKEN          -> GET /metrics
                         OTEL_EXPORTER_OTLP_ENDPOINT  -> trace export
                         LOKI_URL                     -> log shipping
```

Setting none of them is the default and leaves `wardn serve` exactly as
lightweight as if the feature weren't compiled in: just JSON logs to
stdout. `GET /metrics` 404s (not just "unauthenticated") until
`WARDN_METRICS_TOKEN` is set, at which point it also becomes the bearer
token callers must present.

A local Grafana/Prometheus/Tempo/Loki/otel-collector stack to point these
at is one command away, off by default:

```sh
podman-compose --profile observability up -d --build
```

See `.env.example` for the full set of variables and `podman-compose.yml`
for how the stack is wired together.

## Running as a service

`wardn serve` needs to keep running in the background; `wardn service`
manages that for you, under the platform's native, user-level service
manager (no root, no `sudo`):

```sh
wardn service install [--listen <addr>] [--no-linger]  # writes the service
                                                         # definition, enables
                                                         # it, starts it
wardn service status                      # installed? enabled? active?
                                           # linger on? and is it actually
                                           # answering?
wardn service uninstall                   # stops it, removes the
                                           # definition
```

- **Linux:** a `systemd --user` unit at `~/.config/systemd/user/wardn.service`.
  A `systemd --user` manager only runs while you're logged in unless
  *lingering* is on, so `wardn service install` also runs
  `loginctl enable-linger` for you. This is what lets the service survive a
  full reboot on a headless self-hosted box, not just a logout. Pass
  `--no-linger` to skip it (the service then only starts on login); `wardn
  service status` reports whether linger is currently on.
- **macOS:** a launchd `LaunchAgent` at
  `~/Library/LaunchAgents/com.oxhive.wardn.plist`, logging to
  `~/Library/Application Support/wardn/service.log`. There's no linger
  equivalent here: a LaunchAgent only starts on login (interactive or
  auto-login); surviving an unattended reboot needs auto-login enabled for
  this user, since wardn deliberately never installs a root-owned
  LaunchDaemon.

The database path and listen address active at install time (`--db`/
`$WARDN_DB_PATH`, `--listen`/`$WARDN_LISTEN_ADDR`) are baked into the
service definition, since neither systemd user units nor launchd agents
inherit your shell's environment. Re-run `wardn service install` after
changing either; it reinstalls and restarts cleanly.

If you installed via Homebrew: the formula only does `bin.install "wardn"`.
It does not define a `brew services`-managed service, so there's no
conflict with the above. Use `wardn service install`/`status`/`uninstall`
regardless of how the binary was installed (`cargo install`, Homebrew, a
downloaded release binary, or built from source): it's the one, portable
way to run wardn as a background service.

## Storage

A single embedded libSQL database file, no external database server to
install, configure, back up, or keep running alongside the binary. See
`WARDN_SPECIFICATION.md`'s Decision 3 for the full rationale, including why
this comfortably runs on something as modest as a Raspberry Pi 3B.

## How Wardn and Mynd interact

Wardn never touches memory content; it only answers "is this member
authorized to read/write this org's memory, and at what role level" when
Mynd asks, via `wardn serve`'s `/v1/authorize` endpoint. Mynd owns all
memory storage and retrieval; if Wardn is unreachable, Mynd gracefully
skips org-layer memory for that session rather than failing the session
outright. See `WARDN_SPECIFICATION.md`'s "How Wardn and Mynd Actually
Interact" section for the full flow.

## Development

```sh
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt

# Same again without the observability stack compiled in:
cargo test --no-default-features
cargo clippy --all-targets --no-default-features -- -D warnings
```

## License

AGPL-3.0 (see `WARDN_SPECIFICATION.md` Decision 1).
