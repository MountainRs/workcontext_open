"""Read-only acceptance probe for the development archive. Outputs counts, never identities or message text."""
import argparse
import json
import sqlite3
import time
from collections import Counter
from contextlib import closing
from pathlib import Path

QUERY = Path(__file__).resolve().parents[1] / 'crates/desktop/src/local_progress.sql'


def verify(root):
    with closing(sqlite3.connect((root / 'archive.sqlite3').resolve().as_uri() + '?mode=ro', uri=True)) as db:
        db.execute('BEGIN')
        state = dict(db.execute('SELECT key,value FROM archive_state'))
        if not state.get('self_open_id') or not state.get('messages_ready_ms'):
            raise ValueError('消息或本人身份尚未验证')
        now = time.time_ns() // 1_000_000
        start, end = now - 604_800_000, min(now, int(state['messages_ready_ms']))
        me = state['profile'].split(':', 1)[1]
        convs, total = db.execute('SELECT count(distinct cid),count(*) FROM messages WHERE occurred_at BETWEEN ? AND ?', (start,end)).fetchone()
        counts = Counter()
        for row in db.execute(QUERY.read_text(encoding='utf-8'), {'since':start, 'until':end, 'me':me}):
            counts['outgoing' if row[7] else 'incoming'] += 1
            if row[3] is not None:
                counts['directReplies'] += 1
                counts['withQuestion'] += any(ch in (row[4] or '') for ch in '?？')
                counts['withReadableAttachment'] += row[8] is not None
        resources = db.execute('''SELECT a.status, coalesce(a.extraction_status,'not_extracted'), count(*)
            FROM attachments a JOIN messages m USING(cid,mid) WHERE m.occurred_at BETWEEN ? AND ? GROUP BY 1,2''', (start,end)).fetchall()
        documents = []
        if state.get('documents_ready_ms'):
            documents = db.execute('SELECT extension,observation,units FROM recent_document_evidence WHERE modified_ms BETWEEN ? AND ? ORDER BY extension', (start,now)).fetchall()
        return {'messagesThroughMs':end, 'lastFullCycleMs':state.get('last_success_ms'), 'conversations':convs, 'messages':total,
                'signals':dict(counts), 'attachmentsByDownloadAndExtraction':resources,
                'documentIndex':state.get('document_status'), 'documents':documents}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, required=True)
    try:
        print(json.dumps(verify(parser.parse_args().root), ensure_ascii=False, indent=2))
    except (OSError, sqlite3.Error, KeyError, ValueError) as exc:
        parser.exit(1, f'核验未完成：{exc}\n')
