-- A failed delivery is terminal evidence. Continuing a blocked member queue resolves the
-- blocker without rewriting that historical terminal status to `skipped`.
ALTER TABLE chat_message_queue
ADD COLUMN failure_resolved_at TEXT;

CREATE INDEX idx_chat_message_queue_unresolved_failure
    ON chat_message_queue(session_agent_id)
    WHERE status = 'failed' AND failure_resolved_at IS NULL;
