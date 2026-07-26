CREATE TABLE chat_executor_approval_requests (
    id                    BLOB NOT NULL PRIMARY KEY,
    session_id            BLOB NOT NULL,
    session_agent_id      BLOB NOT NULL,
    run_id                BLOB NOT NULL,
    workflow_execution_id BLOB,
    workflow_step_id      BLOB,
    runner                TEXT NOT NULL,
    tool_call_id          TEXT NOT NULL,
    tool_name             TEXT NOT NULL,
    display_input         TEXT NOT NULL DEFAULT '{}',
    options               TEXT NOT NULL DEFAULT '[]',
    status                TEXT NOT NULL DEFAULT 'pending'
                          CHECK (status IN ('pending', 'selected', 'cancelled', 'expired')),
    selected_option_id    TEXT,
    processed_by          TEXT,
    expires_at            TEXT NOT NULL,
    resolved_at           TEXT,
    created_at            TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at            TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    FOREIGN KEY (session_id) REFERENCES chat_sessions(id) ON DELETE CASCADE,
    FOREIGN KEY (session_agent_id) REFERENCES chat_session_agents(id) ON DELETE CASCADE,
    FOREIGN KEY (run_id) REFERENCES chat_runs(id) ON DELETE CASCADE,
    FOREIGN KEY (workflow_execution_id) REFERENCES chat_workflow_executions(id) ON DELETE CASCADE,
    FOREIGN KEY (workflow_step_id) REFERENCES chat_workflow_steps(id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX idx_executor_approval_run_tool
    ON chat_executor_approval_requests(run_id, tool_call_id);

CREATE INDEX idx_executor_approval_session_status
    ON chat_executor_approval_requests(session_id, status, created_at);

CREATE INDEX idx_executor_approval_agent_status
    ON chat_executor_approval_requests(session_agent_id, status);

CREATE INDEX idx_executor_approval_workflow
    ON chat_executor_approval_requests(workflow_execution_id, workflow_step_id, status);
