-- Allow non-lead workflow session members to persist structured Review-node decisions.
PRAGMA foreign_keys = OFF;
COMMIT;
BEGIN TRANSACTION;

CREATE TABLE chat_workflow_step_reviews_new (
    id            BLOB    NOT NULL PRIMARY KEY,
    step_id        BLOB    NOT NULL REFERENCES chat_workflow_steps(id),
    execution_id   BLOB    NOT NULL REFERENCES chat_workflow_executions(id),
    reviewer_type  TEXT    NOT NULL CHECK (reviewer_type IN ('lead', 'reviewer', 'user')),
    reviewer_id    TEXT,
    verdict        TEXT    NOT NULL CHECK (verdict IN ('approved', 'rejected')),
    feedback       TEXT    NOT NULL DEFAULT '',
    review_round   INTEGER NOT NULL DEFAULT 0,
    created_at     TEXT    NOT NULL DEFAULT (datetime('now', 'subsec'))
);

INSERT INTO chat_workflow_step_reviews_new (
    id, step_id, execution_id, reviewer_type, reviewer_id, verdict,
    feedback, review_round, created_at
)
SELECT
    id, step_id, execution_id, reviewer_type, reviewer_id, verdict,
    feedback, review_round, created_at
FROM chat_workflow_step_reviews;

DROP TABLE chat_workflow_step_reviews;
ALTER TABLE chat_workflow_step_reviews_new RENAME TO chat_workflow_step_reviews;

CREATE INDEX idx_workflow_step_reviews_step_id ON chat_workflow_step_reviews(step_id);
CREATE INDEX idx_workflow_step_reviews_execution_id ON chat_workflow_step_reviews(execution_id);
CREATE INDEX idx_workflow_step_reviews_reviewer_type ON chat_workflow_step_reviews(reviewer_type);

PRAGMA foreign_key_check;
COMMIT;
PRAGMA foreign_keys = ON;

-- sqlx workaround due to lack of `-- no-transaction` in sqlx-sqlite.
BEGIN TRANSACTION;
