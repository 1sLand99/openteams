-- Preserve the member identity needed to rebuild an authoritative per-member
-- delivery projection after the queue row itself has been deleted.
ALTER TABLE chat_runtime_outbox ADD COLUMN session_agent_id BLOB;
ALTER TABLE chat_runtime_outbox ADD COLUMN agent_id BLOB;

UPDATE chat_runtime_outbox
SET session_agent_id = (
        SELECT queue.session_agent_id
        FROM chat_message_queue queue
        WHERE queue.id = chat_runtime_outbox.delivery_id
    ),
    agent_id = (
        SELECT queue.agent_id
        FROM chat_message_queue queue
        WHERE queue.id = chat_runtime_outbox.delivery_id
    )
WHERE session_agent_id IS NULL OR agent_id IS NULL;

DROP TRIGGER chat_message_queue_runtime_revision_after_insert;
DROP TRIGGER chat_message_queue_runtime_revision_after_update;
DROP TRIGGER chat_message_queue_runtime_revision_after_delete;

CREATE TRIGGER chat_message_queue_runtime_revision_after_insert
AFTER INSERT ON chat_message_queue
BEGIN
    INSERT INTO chat_session_runtime_revisions (session_id, revision, updated_at)
    VALUES (NEW.session_id, 1, datetime('now', 'subsec'))
    ON CONFLICT(session_id) DO UPDATE SET
        revision = revision + 1,
        updated_at = excluded.updated_at;

    INSERT INTO chat_runtime_outbox (
        session_id,
        revision,
        delivery_id,
        delivery_revision,
        event_type,
        session_agent_id,
        agent_id
    )
    SELECT
        NEW.session_id,
        revision,
        NEW.id,
        NEW.revision,
        'delivery_created',
        NEW.session_agent_id,
        NEW.agent_id
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
        session_id,
        revision,
        delivery_id,
        delivery_revision,
        event_type,
        session_agent_id,
        agent_id
    )
    SELECT
        NEW.session_id,
        revision,
        NEW.id,
        NEW.revision,
        'delivery_updated',
        NEW.session_agent_id,
        NEW.agent_id
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
        session_id,
        revision,
        delivery_id,
        delivery_revision,
        event_type,
        session_agent_id,
        agent_id
    )
    SELECT
        OLD.session_id,
        revision,
        OLD.id,
        OLD.revision,
        'delivery_deleted',
        OLD.session_agent_id,
        OLD.agent_id
    FROM chat_session_runtime_revisions
    WHERE session_id = OLD.session_id;
END;
