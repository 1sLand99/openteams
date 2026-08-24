---
title: "Chat delivery runtime and outbox implementation plan"
description: "Implementation plan for making chat delivery state durable, idempotent, versioned, and recoverable."
---

# Chat delivery runtime and outbox implementation plan

Status: P0 implemented; P1 and P2 pending  
Acceptance source: `docs/qa/chat-delivery-e2e-acceptance.md`  
Primary owners: database delivery ledger, chat runner, chat HTTP API, frontend runtime projection

## Outcome

Every `source_message × session_agent` has one durable delivery record. The database owns queued,
starting, running, approval, stopping, and terminal state. WebSocket events accelerate projection
updates but never provide the only copy of runtime truth.

The implementation must preserve these invariants:

1. A retry with the same `(session_id, client_message_id)` returns the original message and stable
   deliveries without creating another run.
2. Message, resolved targets, delivery rows, runtime revision, and outbox rows commit together.
3. Normal state transitions use expected status and delivery revision. Terminal states are not
   rewritten.
4. Runtime snapshots are complete projections at a monotonic session revision.
5. A stale executor cannot finalize a newer delivery attempt.

## Current baseline

The repository already uses `chat_message_queue` as a durable delivery ledger and enforces a unique
`(chat_message_id, session_agent_id)` key. It also stores a per-delivery revision and attempt number,
binds run creation to delivery/member state in a transaction, and writes session revisions plus
outbox rows from delivery triggers.

Before P0, HTTP sends committed the message first and called `ChatRunner::handle_message` afterward.
Targets, deliveries, and claims therefore committed in separate transactions. A crash or timeout
could leave an idempotent message with no target or delivery, and a retry skipped routing because
the message already existed.

`processing` is a rolling-upgrade read alias for a claimed delivery. Historical rows are correctly
normalised to `starting`, not `running`, because an unbound claimed row has no durable run yet. New
writes must never produce `processing`.

## Scope

### In scope

- Atomic user and agent-protocol message delivery bundles.
- Stable retry responses containing message, deliveries, and the commit revision.
- Strict normal transition rules and a separate CAS-guarded orphan recovery path.
- Immutable failed history while allowing a blocked queue to continue.
- Versioned outbox publication and replay.
- A single frontend delivery reducer and `ConversationItem` projection.
- Lease, heartbeat, and attempt fencing for crash recovery.

### Not in scope

- Replacing `chat_message_queue` with a new table.
- Using `chat_session_agents.state` as delivery truth.
- Treating WebSocket delivery as durable or exactly-once.
- Reworking workflow runtime state, which has its own reducer and event model.

## Delivery states

The target lifecycle is:

```text
queued -> starting -> running <-> waiting_approval -> stopping
                                      |                 |
                                      +-------> terminal+

terminal = completed | failed | cancelled | skipped
```

Startup and executor failures may move an active attempt directly to an appropriate terminal state.
Normal transitions never return an active delivery to `queued`. Only a supervisor recovery operation
may do that after proving the in-memory executor is orphaned, and the write must match the observed
status and revision.

`failed` remains terminal. Continuing a blocked queue sets `failure_resolved_at`; it does not rewrite
the failed record to `skipped`. Queue blocking is derived from unresolved failed rows.

## P0: persistence and state safety

Status: implemented.

### Atomic send transaction

The send path performs the following database writes in one transaction:

1. Claim the explicit user idempotency key when present.
2. Insert the persisted message.
3. Insert or reconcile every resolved `chat_message_target`.
4. Insert one delivery per resolved target.
5. Claim the oldest eligible delivery for each idle target member as `starting`.
6. Read the resulting runtime revision written by delivery triggers.
7. Commit, then emit events and dispatch the claimed rows.

The transaction returns the persisted message, its stable delivery rows, dispatchable claimed rows,
and the commit revision. JSON, multipart, resend, and agent protocol send paths use this entry point.
Multipart failures remove the newly reserved attachment directory.

On an idempotency replay, the service resolves targets from the original message rather than the
new request body. It idempotently fills target or delivery rows missing from legacy split writes and
wakes an unbound `starting` delivery after commit.

### State-machine safety

Normal transition validation rejects active-to-queued and all terminal rewrites. Orphan recovery is
an explicit operation guarded by delivery ID, expected status, and expected revision. Concurrent
stop or finalization therefore wins over a stale recovery observation.

Failed deliveries preserve their terminal status. `failure_resolved_at` records the user's continue
decision, and claim/block/snapshot queries consider only unresolved failures blocking.

### P0 verification gate

- Retry creates one message, target, delivery, idempotency row, and stable delivery ID.
- Target or delivery failure rolls back message, idempotency, revision, and outbox writes.
- A legacy message without routing rows is repaired without changing its message ID.
- An idle send commits a `starting` delivery before runner wake-up.
- Active-to-queued normal transitions and terminal rewrites are rejected.
- Recovery with a stale status/revision cannot overwrite a newer transition.
- Continue preserves `failed`, sets `failure_resolved_at`, and releases the queued successor.

## P1: versioned runtime outbox

Status: pending.

### Publisher

Add a background publisher that claims unpublished `chat_runtime_outbox` rows in session/revision
order, loads the authoritative delivery payload, publishes a typed envelope, and records publication
after successful handoff. Duplicate publication is allowed; missing database state is not.

```text
{ session_id, revision, event_type, delivery }
```

All runtime broadcasts must originate from committed outbox rows. Remove direct delivery-state
broadcasts after consumers have switched.

### Snapshot and replay contract

- A client ignores duplicate or older revisions.
- A revision gap triggers replay from the last applied revision.
- If the requested revision is outside retention, the server returns a complete snapshot.
- A newer snapshot completely replaces the session delivery projection.
- Snapshot failure preserves the current projection and exposes an error.
- WebSocket lag explicitly requests resynchronisation instead of silently dropping events.

### P1 verification gate

- Duplicate, out-of-order, and missing events converge to the database snapshot.
- Publisher restart resumes unpublished rows without losing a revision.
- A stale snapshot cannot replace a newer client projection.
- A disconnected client can recover through replay or snapshot without local runtime storage.

## P1: frontend projection cutover

Status: pending.

Use one reducer partitioned by session and render:

```text
ConversationItem = PersistedMessage | DeliveryCard
```

`DeliveryCard` uses the durable delivery ID as its key. Before the send response, the composer may
hold local `submitting` command state; after the response, persisted deliveries own all queued,
starting, running, approval, stopping, and failure UI.

Only delivery terminal events remove an active card. `message_new` only appends a persisted message.
After shadow comparison shows the new projection matches the backend snapshot, remove placeholder
IDs, run/source/name correlation, hydration display gates, timed reconciliation, dual active-run
merges, and localStorage runtime compensation.

## P2: lease and crash recovery

Status: pending.

Add `lease_token`, `lease_expires_at`, and heartbeat metadata to active deliveries. Increment
`attempt_no` whenever a queued row is claimed. Every bind, heartbeat, stop, and finalization write
must match the active attempt and lease token.

On startup, recover only expired leases. A stale executor holding an earlier token cannot append a
terminal result or create another final message. Recovery emits a typed revisioned event and either
requeues the same delivery ID or finalises it according to the retry policy.

## Pull request sequence

1. P0 database/model changes and atomic message-delivery service.
2. P0 route/protocol cutover, compatibility response fields, and regression tests.
3. P1 outbox publisher, replay endpoint, and versioned WebSocket envelope.
4. P1 frontend reducer cutover behind a feature flag and shadow comparison telemetry.
5. P2 lease fencing, recovery sweeper, and removal of legacy recovery helpers.
6. Compatibility cleanup after all CDD acceptance cases pass in release-like mode.

## Rollout and observability

Track orphan messages without targets, resolved targets without deliveries, duplicate idempotency
attempts, revision gaps, unpublished outbox age, stale recovery CAS misses, expired leases, and old/new
frontend projection divergence.

Keep existing response fields while adding deliveries and revision. Roll back by disabling the new
frontend projection or outbox consumer; do not roll back durable ledger migrations or re-enable split
message/target/delivery writes.

## Completion definition

The work is complete when all cases in `docs/qa/chat-delivery-e2e-acceptance.md` pass independently,
the frontend contains no fake runtime messages or local runtime compensation, outbox replay survives
restart and connection loss, and lease fencing prevents a stale executor from committing a newer
attempt's terminal state.
