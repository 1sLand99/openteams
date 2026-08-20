ALTER TABLE project_work_items
ADD COLUMN parent_id TEXT REFERENCES project_work_items(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS idx_project_work_items_parent_id
    ON project_work_items(parent_id);
