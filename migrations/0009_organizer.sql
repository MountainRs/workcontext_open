-- The archive owns the body. Its reference and journal sequence are sufficient to replay.
ALTER TABLE context_input ADD COLUMN origin_ref TEXT;
ALTER TABLE context_input ADD COLUMN source_version INTEGER NOT NULL DEFAULT 1;
ALTER TABLE context_input ADD COLUMN source_occurred_at INTEGER;
CREATE UNIQUE INDEX ux_input_archive ON context_input(profile,origin_ref);
ALTER TABLE context_record ADD COLUMN organizer_created INTEGER NOT NULL DEFAULT 0 CHECK (organizer_created IN (0,1));
CREATE TABLE organizer_checkpoint (
    profile TEXT PRIMARY KEY, seq INTEGER NOT NULL CHECK (seq>=0), last_organized_at INTEGER, grant_key TEXT
);
CREATE TABLE organizer_job (
    profile TEXT NOT NULL, input_id TEXT NOT NULL, version INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending','claimed','retry','organized','no_record')),
    attempts INTEGER NOT NULL DEFAULT 0, next_at INTEGER NOT NULL DEFAULT 0,
    queued_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER)*1000),
    token TEXT, lease_until INTEGER, last_error TEXT, finished_at INTEGER,
    reply_ref TEXT, reply_version INTEGER, reply_expires_at INTEGER,
    CHECK ((reply_ref IS NULL)=(reply_version IS NULL) AND (reply_ref IS NULL)=(reply_expires_at IS NULL)),
    PRIMARY KEY(profile,input_id),
    FOREIGN KEY(profile,input_id) REFERENCES context_input(profile,id),
    CHECK ((state='claimed')=(token IS NOT NULL)), CHECK ((state='claimed')=(lease_until IS NOT NULL))
);
CREATE INDEX ix_organizer_ready ON organizer_job(profile,state,next_at,lease_until);
CREATE INDEX ix_organizer_reply_expiry ON organizer_job(profile,reply_expires_at) WHERE reply_expires_at IS NOT NULL;
CREATE TRIGGER organizer_pending_insert AFTER INSERT ON context_input WHEN new.processing_state='pending'
BEGIN
    INSERT INTO organizer_job(profile,input_id,version,state) VALUES(new.profile,new.id,new.source_version,'pending');
END;
CREATE TRIGGER organizer_pending_update AFTER UPDATE OF processing_state,source_version ON context_input
WHEN new.processing_state='pending'
BEGIN
    INSERT INTO organizer_job(profile,input_id,version,state) VALUES(new.profile,new.id,new.source_version,'pending')
    ON CONFLICT(profile,input_id) DO UPDATE SET version=excluded.version,state='pending',attempts=0,next_at=0,
        token=NULL,lease_until=NULL,last_error=NULL,finished_at=NULL,queued_at=excluded.queued_at;
END;
UPDATE context_input SET processing_state='pending' WHERE producer IS NULL AND processing_state='organized';
INSERT OR IGNORE INTO organizer_job(profile,input_id,version,state)
    SELECT profile,id,source_version,'pending' FROM context_input WHERE processing_state='pending';
-- Identity survives removal of a facet. A withdrawn observation can be re-established without changing its record ID.
CREATE TABLE organizer_identity (
    profile TEXT NOT NULL,input_id TEXT NOT NULL,delivery TEXT NOT NULL,record_id TEXT NOT NULL,
    PRIMARY KEY(profile,input_id,delivery),
    FOREIGN KEY(profile,input_id) REFERENCES context_input(profile,id),
    FOREIGN KEY(profile,record_id) REFERENCES context_record(profile,id)
);
CREATE TABLE organizer_facet (
    profile TEXT NOT NULL,record_id TEXT NOT NULL,input_id TEXT NOT NULL,slot INTEGER NOT NULL,
    title TEXT NOT NULL,content TEXT NOT NULL,kind TEXT NOT NULL,scope_id TEXT,
    origin_ref TEXT NOT NULL,version INTEGER NOT NULL,delivery TEXT NOT NULL,facets_json TEXT NOT NULL,
    ticket TEXT, reply_ref TEXT, reply_version INTEGER, CHECK ((reply_ref IS NULL)=(reply_version IS NULL)),
    PRIMARY KEY(profile,input_id,slot),
    FOREIGN KEY(profile,input_id) REFERENCES context_input(profile,id),
    FOREIGN KEY(profile,record_id) REFERENCES context_record(profile,id),
    FOREIGN KEY(profile,scope_id) REFERENCES work_thread(profile,id)
);
CREATE INDEX ix_organizer_facet_record ON organizer_facet(profile,record_id);
CREATE INDEX ix_organizer_ticket ON organizer_facet(profile,ticket) WHERE ticket IS NOT NULL;
CREATE INDEX ix_organizer_parent ON organizer_job(profile,reply_ref) WHERE reply_ref IS NOT NULL;
CREATE INDEX ix_organizer_parent_facet ON organizer_facet(profile,reply_ref) WHERE reply_ref IS NOT NULL;

-- Cross-source attachment depends on the destination record's original anchor too.
-- Retire that association and reconsider the independent input if the destination changes.
CREATE TRIGGER organizer_target_changed AFTER UPDATE OF source_version ON context_input
WHEN new.source_version>old.source_version
BEGIN
    UPDATE context_record SET occurred_at=new.source_occurred_at
    WHERE profile=new.profile AND input_id=new.id AND organizer_created=1;
    UPDATE context_input SET processing_state='pending' WHERE profile=new.profile AND id IN (
        SELECT f.input_id FROM organizer_facet f JOIN context_record r ON r.profile=f.profile AND r.id=f.record_id
        WHERE r.profile=new.profile AND r.input_id=new.id AND r.organizer_created=1 AND f.input_id<>new.id
        UNION SELECT input_id FROM organizer_job WHERE profile=new.profile AND reply_ref=new.origin_ref
    );
    DELETE FROM organizer_facet WHERE profile=new.profile AND input_id<>new.id AND record_id IN (
        SELECT id FROM context_record WHERE profile=new.profile AND input_id=new.id AND organizer_created=1
    );
    DELETE FROM organizer_facet WHERE profile=new.profile AND reply_ref=new.origin_ref;
END;
