---
type: ADR
title: Durable session input queue
description: Defines persistence, ordering, claiming, recovery, and operational limits for asynchronous session prompts.
status: Accepted
tags:
  - session
  - queue
  - sqlite
  - durability
generated:
  by: OpenCodeRust contributors
  at: "2026-10-14T00:00:00+09:00"
---

## ADR-0004: Durable session input queue

### Status

Accepted. The implementation is present in the server and storage crates. The
queue-specific storage lifecycle test and workspace validation pass.

### Context

`prompt_async` must acknowledge asynchronous input without losing it when the
server process restarts. An in-memory task queue alone cannot provide that
guarantee. The work also needs per-session ordering, duplicate-claim
protection, and a recovery path for work interrupted by process termination.

### Decision

#### Durable record

Store each asynchronous input in SQLite `session_input` with its prompt,
session ID, delivery mode, serialized model/agent request metadata, status,
and creation time. The metadata column is added to existing databases by a
schema migration. The session foreign key cascades when a session is deleted.

The supported lifecycle is:

| Status | Meaning |
| --- | --- |
| `pending` | Persisted and eligible to run |
| `processing` | Atomically claimed by a worker |
| `promoted` | Prompt execution completed successfully |
| `failed` | Execution cannot proceed or returned an error |

Provider errors are terminal for that queue item; they are recorded as
`failed`, and the next item is allowed to proceed. Transient queue database
read/claim errors trigger a delayed retry.

#### Ordering and claiming

Items are ordered by `created_at`, then SQLite `rowid` to preserve insertion
order when timestamps have millisecond precision and collide. `claim_pending`
uses one conditional `UPDATE`; it claims an item only if no earlier item for
the same session remains `pending` or `processing`. This prevents multiple
workers sharing the database from claiming later prompts while an earlier
prompt is active. Claiming is idempotent: only one caller can transition a
given item from `pending` to `processing`.

The server also serializes prompt execution per session within a process.
These process-local locks complement, but do not replace, the database claim
guard.

#### Message creation and recovery

The queue row is committed before execution begins. When a worker claims the
item, it creates the user message in the session using the queue item ID as
the message ID. If recovery finds that message already present, it reuses it
instead of creating a duplicate. The updated session is then synchronized to
storage.

At storage-backed startup, stale `processing` rows are reset to `pending`.
Both server startup entry points then start the first pending item per
session. Missing or malformed execution metadata is marked `failed` so it
cannot block later items.

#### Single-writer operational constraint

Run one server process against a given SQLite database. The conditional claim
prevents duplicate claims and overlapping queue items, but session snapshots
are held in process memory and synchronized as whole-session state. Multiple
server processes sharing the database can therefore overwrite each other's
session snapshots. The startup reset of `processing` rows also assumes there
is no other live server process using that database.

### Consequences

- Asynchronous input survives process restart after its queue row has been
  committed.
- Same-session queue execution is ordered and the user message ID is stable
  across recovery.
- Queue admission and session snapshot persistence are not one SQLite
  transaction. Recovery relies on the durable queue row and stable message ID
  to recreate or reuse the session message.
- Provider failures are not automatically retried; operators can inspect
  `failed` rows and explicitly re-enqueue work.
- Multiple active server processes per database are unsupported until session
  state is moved to coordinated, transactional storage.

### Verification

The queue lifecycle test covers insertion order, duplicate-claim rejection,
processing-state reset, blocking later items while an earlier item is
processing, and promoting the head before claiming its successor.

Validated with:

```bash
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test --workspace
```

### References

- `crates/opencode-server/src/routes.rs`
- `crates/opencode-server/src/server.rs`
- `crates/opencode-storage/src/repository.rs`
- `crates/opencode-storage/src/schema.rs`
