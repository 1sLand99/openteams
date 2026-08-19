CREATE TABLE application_data_migrations (
    name          TEXT PRIMARY KEY NOT NULL,
    status        TEXT NOT NULL CHECK (status IN ('pending', 'completed', 'failed')),
    error_summary TEXT,
    created_at    TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    updated_at    TEXT NOT NULL DEFAULT (datetime('now', 'subsec'))
);
