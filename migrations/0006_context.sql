-- A NULL profile leaves historical threads in their original local view.
ALTER TABLE work_thread ADD COLUMN profile TEXT;
ALTER TABLE work_thread ADD COLUMN kind TEXT NOT NULL DEFAULT 'project' CHECK (kind IN ('project','domain'));
ALTER TABLE work_thread ADD COLUMN goal TEXT NOT NULL DEFAULT '';
ALTER TABLE work_thread ADD COLUMN background TEXT NOT NULL DEFAULT '';
ALTER TABLE work_thread ADD COLUMN revision INTEGER NOT NULL DEFAULT 1;
CREATE UNIQUE INDEX ux_thread_profile_id ON work_thread(profile,id);
ALTER TABLE ownership_card ADD COLUMN context_scope_id TEXT REFERENCES work_thread(id);
CREATE INDEX ix_card_scope ON ownership_card(profile,context_scope_id);

CREATE TABLE context_input (
    id TEXT PRIMARY KEY NOT NULL DEFAULT ('input-' || lower(hex(randomblob(16)))),
    profile TEXT NOT NULL,
    original TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (profile,id)
);
CREATE TABLE context_record (
    id TEXT PRIMARY KEY NOT NULL DEFAULT ('record-' || lower(hex(randomblob(16)))),
    profile TEXT NOT NULL,
    input_id TEXT NOT NULL,
    title TEXT NOT NULL,
    content TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('event','progress','decision','experience')),
    occurred_at INTEGER,
    created_at INTEGER NOT NULL,
    UNIQUE (profile,id),
    FOREIGN KEY (profile,input_id) REFERENCES context_input(profile,id)
);
CREATE INDEX ix_record_profile_time ON context_record(profile,created_at DESC,id);
CREATE TABLE record_scope (
    profile TEXT NOT NULL,
    record_id TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    PRIMARY KEY (profile,record_id,scope_id),
    FOREIGN KEY (profile,record_id) REFERENCES context_record(profile,id) ON DELETE CASCADE,
    FOREIGN KEY (profile,scope_id) REFERENCES work_thread(profile,id)
);
CREATE INDEX ix_record_scope_lookup ON record_scope(profile,scope_id,record_id);
CREATE TABLE record_ref (
    record_id TEXT NOT NULL REFERENCES context_record(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    material TEXT NOT NULL,
    PRIMARY KEY (record_id,position),
    UNIQUE (record_id,material)
);
