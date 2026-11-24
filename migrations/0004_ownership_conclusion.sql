CREATE TABLE ownership_conclusion (
    card_id TEXT PRIMARY KEY NOT NULL REFERENCES ownership_card(id) ON DELETE CASCADE,
    note TEXT NOT NULL,
    resolved INTEGER NOT NULL CHECK (resolved IN (0, 1)),
    anchors_json TEXT NOT NULL
);
