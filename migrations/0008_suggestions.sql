CREATE TABLE scope_proposal (
    id TEXT PRIMARY KEY NOT NULL DEFAULT ('proposal-' || lower(hex(randomblob(16)))),
    profile TEXT NOT NULL,
    producer TEXT NOT NULL,
    request_id TEXT NOT NULL,
    payload TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','accepted','declined','stale')),
    result_scope_id TEXT,
    created_at INTEGER NOT NULL,
    decided_at INTEGER,
    UNIQUE (profile,producer,request_id),
    FOREIGN KEY (profile,result_scope_id) REFERENCES work_thread(profile,id),
    CHECK ((status='accepted')=(result_scope_id IS NOT NULL)),
    CHECK ((status='pending')=(decided_at IS NULL))
);
CREATE INDEX ix_proposal_pending ON scope_proposal(profile,created_at,id) WHERE status='pending';
