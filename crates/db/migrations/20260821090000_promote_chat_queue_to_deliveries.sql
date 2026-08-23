PRAGMA foreign_keys = OFF;

-- `chat_message_queue` is the durable delivery ledger. Historical versions allowed duplicate
-- rows for the same source message and target member and used `processing` as an internal-only
-- state. Rebuild the table so the delivery identity and lifecycle are enforced by SQLite.
CREATE TABLE chat_message_queue_delivery_new (
    id                    BLOB PRIMARY KEY,
    session_id            BLOB NOT NULL,
    session_agent_id      BLOB NOT NULL,
    agent_id              BLOB NOT NULL,
    chat_message_id       BLOB NOT NULL,
    status                TEXT NOT NULL DEFAULT 'queued'
                            CHECK (status IN (
                                'queued',
                                'starting',
                                'processing',
                                'running',
                                'waiting_approval',
                                'stopping',
                                'failed',
                                'cancelled',
                                'skipped',
                                'completed'
                            )),
    revision              INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    attempt_no            INTEGER NOT NULL DEFAULT 0 CHECK (attempt_no >= 0),
    processing_started_at TEXT,
    run_id                BLOB,
    failure_reason        TEXT,
    created_at            TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at            TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    FOREIGN KEY (session_id) REFERENCES chat_sessions(id) ON DELETE CASCADE,
    FOREIGN KEY (session_agent_id) REFERENCES chat_session_agents(id) ON DELETE CASCADE,
    FOREIGN KEY (agent_id) REFERENCES chat_agents(id) ON DELETE CASCADE,
    FOREIGN KEY (chat_message_id) REFERENCES chat_messages(id) ON DELETE CASCADE,
    FOREIGN KEY (run_id) REFERENCES chat_runs(id) ON DELETE SET NULL
);

-- Keep one authoritative delivery when an older database contains duplicate enqueue attempts.
-- An active delivery wins, an already-completed delivery must never be replayed, and a queued
-- retry wins over a failed/skipped duplicate. The timestamp only breaks ties within that ordering.
--
-- Older schemas also allowed two distinct deliveries to point at the same run, and more than one
-- active delivery for a member. Normalize those corrupt combinations before creating the unique
-- indexes: retain the strongest owner and preserve every conflicting row as `skipped` history.
INSERT INTO chat_message_queue_delivery_new (
    id,
    session_id,
    session_agent_id,
    agent_id,
    chat_message_id,
    status,
    revision,
    attempt_no,
    processing_started_at,
    run_id,
    failure_reason,
    created_at,
    updated_at
)
SELECT
    id,
    session_id,
    session_agent_id,
    agent_id,
    chat_message_id,
    CASE
        WHEN status IN ('starting', 'running') AND (active_rank > 1 OR run_rank > 1)
            THEN 'skipped'
        ELSE status
    END,
    1,
    CASE WHEN status = 'queued' THEN 0 ELSE 1 END,
    processing_started_at,
    CASE
        WHEN status = 'queued' OR run_rank > 1
            OR (status IN ('starting', 'running') AND active_rank > 1)
            THEN NULL
        ELSE run_id
    END,
    CASE
        WHEN status IN ('starting', 'running') AND (active_rank > 1 OR run_rank > 1)
            THEN COALESCE(
                failure_reason,
                'normalized conflicting legacy active delivery during migration'
            )
        ELSE failure_reason
    END,
    created_at,
    updated_at
FROM (
    SELECT
        canonical.*,
        ROW_NUMBER() OVER (
            PARTITION BY session_agent_id,
                CASE WHEN status IN ('starting', 'running') THEN 1 ELSE 0 END
            ORDER BY
                CASE status
                    WHEN 'running' THEN 70
                    WHEN 'starting' THEN 60
                    WHEN 'completed' THEN 50
                    WHEN 'queued' THEN 40
                    WHEN 'failed' THEN 30
                    WHEN 'skipped' THEN 20
                    ELSE 0
                END DESC,
                updated_at DESC,
                id DESC
        ) AS active_rank,
        CASE WHEN run_id IS NULL THEN 1 ELSE ROW_NUMBER() OVER (
            PARTITION BY run_id
            ORDER BY
                CASE status
                    WHEN 'running' THEN 70
                    WHEN 'starting' THEN 60
                    WHEN 'completed' THEN 50
                    WHEN 'queued' THEN 40
                    WHEN 'failed' THEN 30
                    WHEN 'skipped' THEN 20
                    ELSE 0
                END DESC,
                updated_at DESC,
                id DESC
        ) END AS run_rank
    FROM (
        SELECT
            id,
            session_id,
            session_agent_id,
            agent_id,
            chat_message_id,
            CASE status WHEN 'processing' THEN 'starting' ELSE status END AS status,
            processing_started_at,
            run_id,
            failure_reason,
            created_at,
            updated_at
        FROM (
            SELECT
                queue.*,
                ROW_NUMBER() OVER (
                    PARTITION BY chat_message_id, session_agent_id
                    ORDER BY
                        CASE status
                            WHEN 'running' THEN 70
                            WHEN 'processing' THEN 60
                            WHEN 'completed' THEN 50
                            WHEN 'queued' THEN 40
                            WHEN 'failed' THEN 30
                            WHEN 'skipped' THEN 20
                            ELSE 0
                        END DESC,
                        updated_at DESC,
                        id DESC
                ) AS delivery_rank
            FROM chat_message_queue queue
        ) delivery_candidates
        WHERE delivery_rank = 1
    ) canonical
)
;

DROP TABLE chat_message_queue;
ALTER TABLE chat_message_queue_delivery_new RENAME TO chat_message_queue;

CREATE INDEX idx_chat_message_queue_member_created_at
    ON chat_message_queue(session_agent_id, created_at);
CREATE INDEX idx_chat_message_queue_session_id
    ON chat_message_queue(session_id);
CREATE INDEX idx_chat_message_queue_member_status
    ON chat_message_queue(session_agent_id, status);
CREATE INDEX idx_chat_message_queue_chat_message_id
    ON chat_message_queue(chat_message_id);

CREATE UNIQUE INDEX idx_chat_message_queue_delivery_key
    ON chat_message_queue(chat_message_id, session_agent_id);
CREATE UNIQUE INDEX idx_chat_message_queue_run_id
    ON chat_message_queue(run_id)
    WHERE run_id IS NOT NULL;
CREATE UNIQUE INDEX idx_chat_message_queue_one_active
    ON chat_message_queue(session_agent_id)
    WHERE status IN ('starting', 'processing', 'running', 'waiting_approval', 'stopping');

-- A client-generated idempotency key is stored separately from message metadata so a timed-out
-- request can be retried without creating a second user message. Existing metadata is backfilled
-- best-effort; INSERT OR IGNORE deterministically keeps the oldest historical message per key.
CREATE TABLE chat_message_idempotency (
    session_id        BLOB NOT NULL,
    client_message_id TEXT NOT NULL CHECK (length(trim(client_message_id)) > 0),
    message_id        BLOB NOT NULL UNIQUE,
    created_at        TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    PRIMARY KEY (session_id, client_message_id),
    FOREIGN KEY (session_id) REFERENCES chat_sessions(id) ON DELETE CASCADE,
    FOREIGN KEY (message_id) REFERENCES chat_messages(id) ON DELETE CASCADE
        DEFERRABLE INITIALLY DEFERRED
);

INSERT OR IGNORE INTO chat_message_idempotency (session_id, client_message_id, message_id)
SELECT
    session_id,
    trim(json_extract(meta, '$.client_message_id')),
    id
FROM chat_messages
WHERE sender_type = 'user'
  AND json_valid(meta)
  AND json_type(meta, '$.client_message_id') = 'text'
  AND length(trim(json_extract(meta, '$.client_message_id'))) > 0
ORDER BY created_at ASC, id ASC;

-- Every durable delivery mutation advances one session-scoped monotonic revision. The outbox is
-- written by the same SQLite statement/transaction, so broadcasting can happen strictly after
-- commit without making WebSocket delivery the source of truth.
CREATE TABLE chat_session_runtime_revisions (
    session_id  BLOB PRIMARY KEY,
    revision    INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
    updated_at  TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    FOREIGN KEY (session_id) REFERENCES chat_sessions(id) ON DELETE CASCADE
);

INSERT INTO chat_session_runtime_revisions (session_id, revision)
SELECT sessions.id, COUNT(queue.id)
FROM chat_sessions sessions
LEFT JOIN chat_message_queue queue ON queue.session_id = sessions.id
GROUP BY sessions.id;

CREATE TABLE chat_runtime_outbox (
    sequence          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id        BLOB NOT NULL,
    revision          INTEGER NOT NULL CHECK (revision > 0),
    delivery_id       BLOB NOT NULL,
    delivery_revision INTEGER NOT NULL CHECK (delivery_revision > 0),
    event_type        TEXT NOT NULL
                          CHECK (event_type IN (
                              'delivery_created',
                              'delivery_updated',
                              'delivery_deleted'
                          )),
    created_at        TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    published_at      TEXT,
    UNIQUE (session_id, revision),
    FOREIGN KEY (session_id) REFERENCES chat_sessions(id) ON DELETE CASCADE
);

CREATE INDEX idx_chat_runtime_outbox_unpublished
    ON chat_runtime_outbox(session_id, revision)
    WHERE published_at IS NULL;

CREATE TRIGGER chat_message_queue_runtime_revision_after_insert
AFTER INSERT ON chat_message_queue
BEGIN
    INSERT INTO chat_session_runtime_revisions (session_id, revision, updated_at)
    VALUES (NEW.session_id, 1, datetime('now', 'subsec'))
    ON CONFLICT(session_id) DO UPDATE SET
        revision = revision + 1,
        updated_at = excluded.updated_at;

    INSERT INTO chat_runtime_outbox (
        session_id, revision, delivery_id, delivery_revision, event_type
    )
    SELECT NEW.session_id, revision, NEW.id, NEW.revision, 'delivery_created'
    FROM chat_session_runtime_revisions
    WHERE session_id = NEW.session_id;
END;

CREATE TRIGGER chat_message_queue_runtime_revision_after_update
AFTER UPDATE ON chat_message_queue
BEGIN
    INSERT INTO chat_session_runtime_revisions (session_id, revision, updated_at)
    VALUES (NEW.session_id, 1, datetime('now', 'subsec'))
    ON CONFLICT(session_id) DO UPDATE SET
        revision = revision + 1,
        updated_at = excluded.updated_at;

    INSERT INTO chat_runtime_outbox (
        session_id, revision, delivery_id, delivery_revision, event_type
    )
    SELECT NEW.session_id, revision, NEW.id, NEW.revision, 'delivery_updated'
    FROM chat_session_runtime_revisions
    WHERE session_id = NEW.session_id;
END;

CREATE TRIGGER chat_message_queue_runtime_revision_after_delete
AFTER DELETE ON chat_message_queue
BEGIN
    INSERT INTO chat_session_runtime_revisions (session_id, revision, updated_at)
    VALUES (OLD.session_id, 1, datetime('now', 'subsec'))
    ON CONFLICT(session_id) DO UPDATE SET
        revision = revision + 1,
        updated_at = excluded.updated_at;

    INSERT INTO chat_runtime_outbox (
        session_id, revision, delivery_id, delivery_revision, event_type
    )
    SELECT OLD.session_id, revision, OLD.id, OLD.revision, 'delivery_deleted'
    FROM chat_session_runtime_revisions
    WHERE session_id = OLD.session_id;
END;

PRAGMA foreign_keys = ON;
