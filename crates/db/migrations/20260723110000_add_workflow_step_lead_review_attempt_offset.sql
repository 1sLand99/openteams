ALTER TABLE chat_workflow_steps
ADD COLUMN lead_review_attempt_offset INTEGER NOT NULL DEFAULT 0;
