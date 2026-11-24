-- Each version is the complete desktop-authored override. NULL follows the source;
-- [] explicitly removes the association, including subsequent organizer suggestions.
CREATE TABLE record_correction (
    profile TEXT NOT NULL, record_id TEXT NOT NULL, version INTEGER NOT NULL CHECK(version > 0),
    content TEXT, attribution TEXT CHECK(attribution IN ('reported','inference','confirmed')),
    scope_ids TEXT CHECK(scope_ids IS NULL OR (json_valid(scope_ids) AND json_type(scope_ids)='array')),
    evidence_versions TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(evidence_versions) AND json_type(evidence_versions)='object'),
    corrected_at INTEGER NOT NULL,
    PRIMARY KEY(profile,record_id,version),
    FOREIGN KEY(profile,record_id) REFERENCES context_record(profile,id) ON DELETE CASCADE
);
CREATE VIEW current_record_correction AS SELECT c.* FROM record_correction c
WHERE NOT EXISTS (SELECT 1 FROM record_correction newer WHERE newer.profile=c.profile
    AND newer.record_id=c.record_id AND newer.version>c.version);
CREATE VIEW effective_record_scope AS
SELECT s.profile,s.record_id,s.scope_id FROM record_scope s
WHERE NOT EXISTS (SELECT 1 FROM current_record_correction c WHERE c.profile=s.profile AND c.record_id=s.record_id AND c.scope_ids IS NOT NULL)
UNION ALL
SELECT c.profile,c.record_id,j.value FROM current_record_correction c,json_each(c.scope_ids) j;

ALTER TABLE context_record ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;
CREATE TRIGGER correction_revision AFTER INSERT ON record_correction BEGIN
    UPDATE context_record SET revision=new.version WHERE profile=new.profile AND id=new.record_id;
END;
CREATE TRIGGER facet_revision_insert AFTER INSERT ON organizer_facet BEGIN
    UPDATE context_record SET revision=revision+1 WHERE profile=new.profile AND id=new.record_id;
END;
CREATE TRIGGER facet_revision_delete AFTER DELETE ON organizer_facet BEGIN
    UPDATE context_record SET revision=revision+1 WHERE profile=old.profile AND id=old.record_id;
END;
CREATE TRIGGER facet_revision_update AFTER UPDATE ON organizer_facet BEGIN
    UPDATE context_record SET revision=revision+1 WHERE (profile,id) IN ((old.profile,old.record_id),(new.profile,new.record_id));
END;
