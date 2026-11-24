ALTER TABLE context_input ADD COLUMN producer TEXT;
ALTER TABLE context_input ADD COLUMN request_id TEXT;
ALTER TABLE context_input ADD COLUMN payload_hash TEXT;
ALTER TABLE context_input ADD COLUMN processing_state TEXT NOT NULL DEFAULT 'organized';
CREATE UNIQUE INDEX ux_context_external_request ON context_input(profile,producer,request_id)
    WHERE producer IS NOT NULL;
ALTER TABLE context_record ADD COLUMN reported_result TEXT;
ALTER TABLE context_record ADD COLUMN inference TEXT;
ALTER TABLE context_record ADD COLUMN open_questions TEXT NOT NULL DEFAULT '[]';
ALTER TABLE context_record ADD COLUMN scope_hints TEXT NOT NULL DEFAULT '[]';
ALTER TABLE record_ref ADD COLUMN summary TEXT;
CREATE TABLE record_related (
    profile TEXT NOT NULL,
    record_id TEXT NOT NULL,
    related_record_id TEXT NOT NULL,
    PRIMARY KEY (profile,record_id,related_record_id),
    FOREIGN KEY (profile,record_id) REFERENCES context_record(profile,id) ON DELETE CASCADE,
    FOREIGN KEY (profile,related_record_id) REFERENCES context_record(profile,id)
);
CREATE INDEX ix_context_record_input ON context_record(profile,input_id);
