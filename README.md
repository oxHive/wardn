# wardn

Open source org & access layer for [`Mynd`](https://github.com/oxhive/mynd)
— a memory MCP server for AI coding agents.

Wardn gives Mynd organization structure: creating an org, inviting and
removing members, and assigning roles that control who can read and write
an org's shared (Layer 3) memory. It is not a billing system, not a
multi-tenant SaaS platform, and not a personal memory sync server — see
`WARDN_SPECIFICATION.md` for the full design and the "What Wardn Is NOT"
section in particular.

```
Mynd solves:   "Claude remembers you across sessions"
Wardn solves:  "your team can share and control access to memory,
                together, under your own roof if you want"
```

**Status:** implements the full CLI surface (org/member/role/key
management), a local libSQL-backed store, and `wardn serve`, the narrow
HTTP authorization API Mynd calls before touching org-layer memory. No
billing, no multi-org routing, no GUI — that's all deliberately out of
scope for this repository (see `WARDN_SPECIFICATION.md`).

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
```

Every operation a self-hosted team needs is available via this CLI — a
paid, closed-source GUI (`console`) exists as a separate, optional product
for people who'd rather click than type, but it holds no capability the
CLI lacks. See Decision 5 in `WARDN_SPECIFICATION.md`.

## Roles

```
admin       — full control: invite/remove members, change roles,
              read + write org-layer memory, rename/delete the org
member      — read + write org-layer memory, cannot manage membership
read_only   — read org-layer memory only, cannot write, cannot
              manage membership
```

## Storage

A single embedded libSQL database file — no external database server to
install, configure, back up, or keep running alongside the binary. See
`WARDN_SPECIFICATION.md`'s Decision 3 for the full rationale, including why
this comfortably runs on something as modest as a Raspberry Pi 3B.

## How Wardn and Mynd interact

Wardn never touches memory content — it only answers "is this member
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
```

## License

AGPL-3.0 (see `WARDN_SPECIFICATION.md` Decision 1).
