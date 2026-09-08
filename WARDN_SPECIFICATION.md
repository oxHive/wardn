# Wardn — Specification
## Open Source Org & Access Layer for Mynd
*Public repository: github.com/oxhive/warden*
*Binary name: `wardn` (deliberately shortened, not a typo)*
*License: open source (see Decision 1 below)*

---

## Revision Note

This document replaces `HIVEMIND_GATEWAY_SPEC.md` in full. That
earlier document described `hivemind-gateway` as a closed-source,
proprietary, PostgreSQL-backed, multi-tenant billing infrastructure
binary. **None of that describes Wardn.** Wardn is a different,
smaller, open source product born from deliberately stripping the
original gateway concept down to a single responsibility: organization
and access management. If you have the old gateway spec open anywhere,
discard it — this document is authoritative.

Naming history, for anyone tracing decisions across this project:
`hivemind-gateway` → `hivewarden` → `HiveWarden` → `Oxhive Warden`
→ **`Wardn`** (final). The memory engine itself, `HiveMind`, was
separately renamed to **`Mynd`** across this same revision — see
`MYND_SPEC.md` (formerly `HIVEMIND_SPEC.md`) for that product.

---

## What Wardn Is

Wardn is a small, self-hostable, open source service that gives
**Mynd** organization structure: creating an org, inviting and
removing members, and assigning roles that control who can read and
write an org's shared Layer 3 memory. It is entirely independent of
billing, hosting, or any commercial concern — those live elsewhere
(see "What Wardn Is Not," below).

```
Mynd solves:   "Claude remembers you across sessions"
Wardn solves:  "your team can share and control access to memory,
                together, under your own roof if you want"
```

---

## What Wardn Is NOT

Stated explicitly because the original gateway design conflated all
of this into one binary, and it's important this doesn't happen
again by accident:

```
✗ Not a billing system — no Stripe, no plans, no subscriptions
✗ Not a multi-tenant SaaS platform — does not serve multiple
   unrelated customers from one running instance
✗ Not a usage metering system — nothing here exists to bill against
✗ Not a rate limiter tied to commercial tiers
✗ Not the thing that decides who pays Oxhive money — that is
   entirely Oxhive's own internal concern, handled by infrastructure
   Wardn has no knowledge of (see "Oxhive's Own Infrastructure" below)
✗ Not a personal memory sync server — syncing your own personal
   (Layer 1) or workspace (Layer 2) memory across your own devices
   is entirely Mynd's job, handled independently of Wardn. See
   "Wardn vs Personal Sync" immediately below — this is worth
   reading even for a solo self-hoster with no team at all.
```

---

## Wardn vs Personal Sync — A Common Point of Confusion

It's natural to assume that if you're self-hosting for yourself, with
an "organization" that has exactly one member (you), Wardn should also
be the thing that centralizes your personal and project memory across
your own devices. **It is not, and should not be used that way.**

Personal (Layer 1) and workspace (Layer 2) memory sync is handled
entirely by Mynd's own sync mechanism — pointing every device's Mynd
instance at a self-hosted sqld/libSQL primary (or Mynd running in its
own serve/replica-primary mode, per `MYND_SPEC_ADDENDUM.md`). This
works completely independently of whether Wardn exists, is running,
or has ever been installed at all.

```
For syncing YOUR OWN personal + project memory across YOUR OWN
devices (no org involved):
  → Run a self-hosted sqld/libSQL primary, or Mynd's own
    serve/replica-primary mode
  → Point every device's Mynd config at it
  → Wardn is not part of this path at all

For giving a TEAM (even a team of one, today) controlled,
role-based access to SHARED org-layer memory:
  → Run wardn serve
  → wardn org create, invite members, assign roles
  → Mynd checks with Wardn before reading/writing org-layer memory
```

### Why Run Both, Even as a Solo Self-Hoster on One Device

Even if your org currently has exactly one member (you), it is
correct — not overkill — to run these as two separate processes with
two separate database files, even co-located on the same device:

```
Process 1: sqld / Mynd serve-mode   →  your personal sync target
Process 2: wardn serve              →  your org's authorization service
                                        (org membership: currently
                                        just you)
```

The reason this separation matters even at a membership of one: Layer
3 (org memory) is structurally *shareable* memory by design, even
when nobody else is sharing it with you yet. Keeping it on a genuinely
separate service from your personal memory means that the day you
invite a co-founder or teammate — via a single `wardn org invite`
command — nothing about the architecture needs to change, and there is
no risk of a new org member ever gaining accidental access to memory
that was always meant to be yours alone. If Wardn also handled
personal sync, that boundary would have to be untangled later instead
of being correct from day one.

The operational cost of running two small processes instead of one is
low — both are lightweight libSQL-backed services (see Decision 3's
Raspberry Pi assessment), and both can be managed with the same
systemd-service pattern already used for Mynd's self-hosted
deployment. This is a deliberate, worthwhile tradeoff, not an
inefficiency to optimize away later.

---

## Decision 1 — License

Wardn is open source. Use the same license as Mynd (AGPL-3.0) unless
a specific reason emerges to diverge — keeping both under one license
avoids a second license-boundary problem the way `console` required
careful handling relative to Mynd's dashboard. Confirm before first
publish; treat AGPL-3.0 as the default unless changed deliberately.

---

## Decision 2 — Single Org Per Instance

Each running `wardn` instance manages exactly **one** organization.
There is no multi-org routing, no tenant resolution, no "which org
does this request belong to" logic anywhere in the open source binary.

```
Self-hosted team:
  Runs one wardn instance → manages their one org → done.
  Wants a second, unrelated org? Run a second wardn instance,
  pointed at a second local database file. Two instances, two
  processes, two ports, zero shared state, zero routing complexity.
```

This is deliberately the simplest possible design. It matches the
real, common case (a single team self-hosting for themselves) without
adding architecture that only pays off at a scale open source Wardn
is not meant to serve.

**Multi-org support exists, but not here.** See "Oxhive's Own
Infrastructure" below — Oxhive achieves multi-org hosting by running
*many single-org Wardn instances*, not by adding multi-org logic to
the Wardn binary itself. This was a deliberate architectural choice
(Option A from the broader design discussion) specifically to avoid
forking Wardn's codebase or introducing a second, closed-source
variant of it.

---

## Decision 3 — Storage: libSQL, Not PostgreSQL

The original gateway spec used PostgreSQL as a control-plane database
for users, API keys, subscriptions, org membership, and usage events
at multi-tenant SaaS scale. None of that scale exists in open source
Wardn anymore — a single self-hosted org's data (org record, member
list, roles, API keys for auth) is small, relational, and entirely
appropriate for the same embedded database approach Mynd already uses.

```
Wardn storage: libSQL (same crate, same approach as Mynd)
  ~/.local/share/wardn/org.db     (default local path convention)
  or wherever the operator configures it

NOT PostgreSQL. Wardn has no external database dependency at all.
```

### Why This Matters Practically

This is what makes Wardn genuinely lightweight to self-host — no
separate database server to install, configure, back up, or keep
running alongside the binary. A single binary, a single file, the
same operational simplicity Mynd already offers.

### Raspberry Pi Suitability

With PostgreSQL removed, Wardn is comfortably suited to a Raspberry
Pi 3B (quad-core Cortex-A53 @ 1.2GHz, 1GB RAM) or similar low-power
hardware:

```
CPU load:     Trivial for Wardn's workload (occasional CLI-driven
              org/member/role operations, light auth checks on
              incoming requests) — nowhere near CPU-bound
RAM:          A Rust binary + embedded libSQL connection should sit
              in the tens of megabytes, not hundreds. Comfortable
              headroom on a 1GB device when Wardn runs alone.
              If co-located on the same Pi as a Mynd server process,
              measure actual combined memory footprint before
              assuming headroom — don't extrapolate from Rust's
              general reputation for leanness alone.
Storage:      SD card wear from repeated small writes is the one
              real long-term consideration, not a blocker. WAL mode
              (same PRAGMA convention as Mynd — see Decision 4)
              writes more frequently than plain journal mode; this
              is a known tradeoff worth being aware of over months
              of uptime on SD storage specifically, not something
              that prevents running Wardn on a Pi today.
```

**Verdict: yes, a Pi 3B runs Wardn comfortably**, in the same class of
workload as other small self-hosted single-purpose services people
already run on hardware this modest.

---

## Decision 4 — Same Database Conventions as Mynd

For consistency across the two products (and because Claude Code will
likely be working on both), Wardn should mirror Mynd's established
libSQL conventions exactly:

```sql
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA foreign_keys=ON;
PRAGMA busy_timeout=5000;
```

Applied immediately after opening the connection, same as specified
for Mynd in `MYND_SPEC_ADDENDUM.md` (formerly `HIVEMIND_SPEC_ADDENDUM.md`).

---

## Decision 5 — CLI Only in Open Source; GUI Is the Paid Layer

This is the core monetization mechanism for the whole Wardn/console
relationship, and it does not gate any *capability* — only the
*interface*.

```
Open source wardn (free, full capability):
  wardn org create --name "Acme Corp"
  wardn org invite user@example.com --role member
  wardn org members list
  wardn org members remove <user_id>
  wardn org role set <user_id> admin
  wardn status

  Every operation a self-hosted team needs is available via CLI.
  Nothing is held back. A technically comfortable team loses
  ZERO functionality by never touching a paid product.

Paid layer (console, closed source, separate product):
  A polished web GUI — member tables, invite forms, role dropdowns —
  for the SAME underlying operations, for people who'd rather click
  than type commands, or who want to hand a non-technical teammate
  a shareable link instead of terminal access.
```

See `CONSOLE_SPEC.md` (or equivalent, not yet written in full) for
console's own scope — console talks to Wardn's API the same way the
CLI does; it holds no special server-side capability the CLI lacks.

### "CLI Only" Does Not Mean "No API" — Why Wardn Still Needs One

It's easy to read Decision 5 as "Wardn has no API in open source,"
but that's not what it says, and the distinction matters. "CLI-only"
describes the **human administrative interface** — the free version
gives you a terminal, the paid version additionally gives you a GUI.
It says nothing about whether Wardn needs a way to be called
*programmatically*, and it does, for a reason unrelated to the
CLI/GUI question entirely.

Every time a Mynd session touches org-layer memory, Mynd must ask
Wardn, in real time, "is this member authorized to read/write this
org's memory, and at what role level" (see "How Wardn and Mynd
Actually Interact," above). Mynd is a separate running process — often
on an entirely different device — so this cannot be satisfied by
shelling out to a CLI command and parsing its output; it requires a
proper request/response API. This need exists **regardless of the
CLI/GUI decision** — it would be true even if console (the paid GUI)
never existed at all.

```
wardn serve   — already listed in the CLI surface below — is this
                API. Running it starts the HTTP authorization
                service that Mynd calls during sessions.
```

So Wardn has exactly one real API surface underneath everything:

```
The CLI (wardn org invite, etc.)
  → operates directly on the local database, OR talks to a
    locally-running `wardn serve` if one is active

Mynd, during a session
  → calls the running `wardn serve` API to check authorization

Console (paid GUI), if used
  → also calls the same `wardn serve` API — console holds no
    privileged access the API itself doesn't expose to anyone else
```

There is no separate "administrative API" distinct from this — the
API's scope is narrow and specific (authorization checks), not a
general-purpose management interface. The CLI remains the only free,
full-capability way for a *human* to administer an org; the API exists
underneath it because Mynd (and, for paying customers, console) are
*programs*, not humans, and programs need a protocol, not a terminal.

---

## Data Model

```sql
CREATE TABLE org (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    created_at  INTEGER NOT NULL
    -- single row table in practice, since each instance = one org,
    -- but modeled as a table rather than a config value in case
    -- that assumption ever needs to change later
);

CREATE TABLE members (
    id          TEXT PRIMARY KEY,
    email       TEXT UNIQUE NOT NULL,
    role        TEXT NOT NULL,       -- 'admin' | 'member' | 'read_only'
    invited_by  TEXT REFERENCES members(id),
    joined_at   INTEGER NOT NULL,
    removed_at  INTEGER              -- nullable, soft-delete on removal
);

CREATE TABLE api_keys (
    id          TEXT PRIMARY KEY,
    member_id   TEXT NOT NULL REFERENCES members(id),
    key_hash    TEXT UNIQUE NOT NULL,   -- argon2 hash, never plaintext
    key_prefix  TEXT NOT NULL,          -- for display, e.g. "wd_a1b2..."
    created_at  INTEGER NOT NULL,
    last_used_at INTEGER,
    revoked_at  INTEGER
);
```

No `plan`, `subscription`, `usage_events`, or `rate_limit` tables —
none of that exists in open source Wardn's data model at all.

---

## Role Semantics

```
admin       — full control: invite/remove members, change roles,
               read + write org-layer memory, rename/delete the org
member      — read + write org-layer memory, cannot manage membership
read_only   — read org-layer memory only, cannot write, cannot
               manage membership
```

Enforcement happens at the point Wardn authorizes a request from
Mynd for org-layer memory access — Wardn doesn't store or process
memory content itself, it only answers "is this member allowed to
read/write this org's memory" when Mynd asks.

---

## How Wardn and Mynd Actually Interact

Wardn does not touch memory content. Mynd owns all memory storage,
retrieval, and the `hivemind_session_start` (now `mynd_session_start`)
hook logic. Wardn's only job relative to Mynd is answering
authorization questions.

```
Mynd, when a session involves org-layer memory:
  1. Reads org connection details from .mynd.toml / config
  2. Sends a request to Wardn: "can member X read org Y's memory?"
  3. Wardn checks its local members/roles table, responds yes/no
  4. If yes, Mynd proceeds to read/write the org's memory database
     (a separate libSQL database Mynd manages — see MYND_SPEC_ADDENDUM.md
     section 5 for how org-layer memory storage itself works)
  5. If no (or Wardn unreachable), Mynd silently skips org-layer
     memory for this session — same graceful degradation pattern
     already specified for Mynd's org-layer handling
```

Wardn is an authorization service, not a memory store. This is an
important distinction from the original gateway design, where the
gateway sat *between* the client and the memory database as a proxy.
In this simplified design, Wardn only answers "is this allowed" —
Mynd still owns talking to the actual org memory database directly.

---

## CLI Surface

```bash
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
wardn status                        # org name, member count, uptime,
                                     # storage path, whether running
wardn serve                         # start the HTTP service that
                                     # Mynd instances authorize against
```

`wardn serve` is the long-running process; the other commands are
one-off administrative operations run against the same local database,
similar in spirit to how database migration CLIs work alongside a
running server process.

---

## Oxhive's Own Infrastructure (Not Part of Wardn Itself)

This section describes how Oxhive achieves *multi-org* hosting for
paying customers, entirely outside the open source `wardn` binary,
per the Option A decision made during design discussion.

```
Oxhive's private infrastructure (closed source, never distributed):
  Runs MANY separate instances of the unmodified open source
  `wardn` binary — one per paying customer org, each with its own
  database file, each single-org exactly as the open source binary
  already works.

  A thin internal routing/orchestration layer (part of console's
  backend, or a small separate internal service) tracks which
  customer maps to which running wardn instance, and directs
  requests accordingly.

  This orchestration layer is where PostgreSQL, billing, Stripe,
  usage metering, and customer account management actually live —
  entirely within Oxhive's own private infrastructure, never inside
  the wardn binary, never distributed, never something a self-hosted
  user's copy of wardn contains or needs to know about.
```

This achieves the original goal (Oxhive can host multi-org access for
paying customers) without ever forking Wardn's codebase, without a
closed-source "wardn-pro" variant, and without reintroducing the
billing/multi-tenancy complexity that was deliberately stripped out
of the open source product.

---

## What Replaces the Old "Admin API"

The original gateway spec had an internal Admin API for Oxhive to
manage users/orgs at scale. That concept now splits cleanly:

```
For a self-hosted org's own admin needs:
  → Wardn's own CLI commands (wardn org invite, wardn role set, etc.)
    ARE the admin interface. No separate API needed.

For Oxhive's internal multi-customer orchestration:
  → Lives entirely in Oxhive's private infrastructure (above),
    operating on top of many independent wardn instances. This is
    Oxhive's own tooling, not part of the Wardn product or spec.
```

---

## Dependencies

```toml
[dependencies]
libsql = { version = "0.6", features = ["core"] }
# "core" only — Wardn does not need replication/replica mode itself,
# since each instance is a single, standalone, single-org store.
# (Contrast with Mynd, which needs "replication" for sync.)

tokio = { version = "1", features = ["full"] }
axum = "0.7"                        # HTTP service for wardn serve
argon2 = "0.5"                      # API key hashing
serde = { version = "1", features = ["derive"] }
serde_json = "1"
clap = { version = "4", features = ["derive"] }
tracing = "0.1"
tracing-subscriber = "0.3"

# No sqlx, no PostgreSQL driver, no Stripe client — none of this
# belongs in the open source wardn binary.
```

---

## Repository and Distribution

```
github.com/oxhive/warden        (public, open source, AGPL-3.0
                                  pending Decision 1's confirmation)
  Binary name: wardn
  Crate name: TBD — check crates.io availability before finalizing
              (likely "oxwarden" or similar if "wardn"/"warden" is
              taken, following the same pattern used for
              oxhivemind/hivemind)
```

Distribution follows the same pattern already established for Mynd:
`cargo install`, plus pre-built GitHub Release binaries for platforms
without a Rust toolchain, following the same GitHub Actions release
workflow already specified for Mynd.

---

## Build Sequence

```
Step 1:  Data model + libSQL setup (org, members, api_keys tables)
Step 2:  wardn init — first-run setup
Step 3:  Org CRUD CLI commands
Step 4:  Member invite/remove CLI commands
Step 5:  Role assignment CLI commands
Step 6:  API key generation/revocation (needed for Mynd to
         authenticate authorization requests against Wardn)
Step 7:  wardn serve — the HTTP authorization service Mynd calls
Step 8:  wardn status
Step 9:  Test end-to-end: Mynd configured with org-layer memory,
         pointed at a running wardn instance, confirm authorization
         flow works and gracefully degrades when Wardn is unreachable
```

Do not build console, multi-org support, or any billing-adjacent
feature as part of this sequence — all of that is explicitly out of
scope for the open source `warden` repository.

---

*Reference documents:*
- *MYND_SPEC.md (formerly HIVEMIND_SPEC.md) — the memory engine Wardn*
  *provides org/access control for*
- *MYND_SPEC_ADDENDUM.md (formerly HIVEMIND_SPEC_ADDENDUM.md) —*
  *section 5 covers how Mynd's org-layer memory storage and graceful*
  *degradation work, which Wardn's authorization responses feed into*
