---
type: Reference
description: SQLite database、schema migration、repository を説明する。
tags: [storage, sqlite, persistence, crate]
---

# opencode-storage

`opencode-storage` provides SQLite persistence: database setup and repository access.

## Responsibilities

- Database connection and migrations
- Session, message, todo, and durable session-input repository implementations
- Unified storage error model

## Module structure

- `database.rs` – DB init, connection management
- `schema.rs` – Table definitions and migrations
- `repository.rs` – Session/Message/Todo repositories

## Key exports

- `Database`
- `DatabaseError`
- `SessionRepository`
- `MessageRepository`
- `TodoRepository`
- `InputRepository`

## Development notes

- Schema changes must remain compatible across migrations
- Keep transaction boundaries clear in repository APIs
- Consider indexes and query cost for hot read/write paths
- Session input rows use `pending`, `processing`, `promoted`, and `failed`
  states; claims enforce per-session FIFO ordering in the database

See [ADR-0004](adr/adr-0004-durable-session-input-queue.md) for queue
recovery semantics and the single-server-per-database constraint.

## Validation

```bash
cargo check -p opencode-storage
```
