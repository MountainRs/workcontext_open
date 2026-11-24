-- Candidates reserve an account-local identity. A formal item exists only after a human action.
CREATE TABLE ownership_card (
    id TEXT PRIMARY KEY NOT NULL DEFAULT ('flow-' || lower(hex(randomblob(16)))),
    profile TEXT NOT NULL,
    candidate_json TEXT NOT NULL,
    present INTEGER NOT NULL DEFAULT 1 CHECK (present IN (0, 1)),
    revision INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX ix_ownership_profile ON ownership_card(profile);

-- Only the most recent move on a card can be undone. The event log remains append-only.
CREATE TABLE ownership_undo (
    token TEXT PRIMARY KEY NOT NULL DEFAULT (lower(hex(randomblob(16)))),
    card_id TEXT NOT NULL UNIQUE REFERENCES ownership_card(id) ON DELETE CASCADE,
    revision INTEGER NOT NULL,
    before_json TEXT NOT NULL
);

-- All writers, including item correction, follow-up and judge, invalidate stale views/receipts.
CREATE TRIGGER tr_flow_item_ai AFTER INSERT ON work_item BEGIN
    UPDATE ownership_card SET revision = revision + 1 WHERE id = NEW.id;
END;
CREATE TRIGGER tr_flow_item_au AFTER UPDATE ON work_item BEGIN
    UPDATE ownership_card SET revision = revision + 1 WHERE id = NEW.id;
END;
CREATE TRIGGER tr_flow_item_ad AFTER DELETE ON work_item BEGIN
    UPDATE ownership_card SET revision = revision + 1 WHERE id = OLD.id;
END;
CREATE TRIGGER tr_flow_event_ai AFTER INSERT ON work_event WHEN NEW.target_type = 'work_item' BEGIN
    UPDATE ownership_card SET revision = revision + 1 WHERE id = NEW.target_id;
END;
CREATE TRIGGER tr_flow_override_ai AFTER INSERT ON field_override BEGIN
    UPDATE ownership_card SET revision = revision + 1 WHERE id = NEW.target_id;
END;
CREATE TRIGGER tr_flow_override_au AFTER UPDATE ON field_override BEGIN
    UPDATE ownership_card SET revision = revision + 1 WHERE id = NEW.target_id;
END;
CREATE TRIGGER tr_flow_override_ad AFTER DELETE ON field_override BEGIN
    UPDATE ownership_card SET revision = revision + 1 WHERE id = OLD.target_id;
END;
CREATE TRIGGER tr_flow_evidence_ai AFTER INSERT ON link_evidence WHEN NEW.target_type = 'work_item' BEGIN
    UPDATE ownership_card SET revision = revision + 1 WHERE id = NEW.target_id;
END;
CREATE TRIGGER tr_flow_evidence_au AFTER UPDATE ON link_evidence WHEN NEW.target_type = 'work_item' BEGIN
    UPDATE ownership_card SET revision = revision + 1 WHERE id = NEW.target_id;
END;
CREATE TRIGGER tr_flow_evidence_ad AFTER DELETE ON link_evidence WHEN OLD.target_type = 'work_item' BEGIN
    UPDATE ownership_card SET revision = revision + 1 WHERE id = OLD.target_id;
END;
