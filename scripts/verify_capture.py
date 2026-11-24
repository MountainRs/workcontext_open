"""Verify captured evidence in the development archive. Emits counts only, no bodies, IDs, or file names."""
import argparse
import hashlib
import json
import re
import sqlite3
import time
from collections import Counter
from contextlib import closing
from pathlib import Path


def verify(root):
    with closing(sqlite3.connect((root/'archive.sqlite3').resolve().as_uri()+'?mode=ro', uri=True)) as db:
        db.execute('BEGIN')
        state = dict(db.execute('SELECT key,value FROM archive_state'))
        start = time.time_ns()//1_000_000 - 7*86_400_000
        end = int(state['messages_ready_ms'])
        window = ' FROM messages m WHERE m.occurred_at>=? AND m.occurred_at<?'
        args = (start,end)
        def by_window(table, expr):
            return db.execute('SELECT '+expr+' FROM '+table+' a JOIN messages m USING(cid,mid) WHERE m.occurred_at>=? AND m.occurred_at<? GROUP BY 1',args).fetchall()
        counts = db.execute('SELECT count(*),count(distinct cid)'+window,args).fetchone()
        captured = db.execute('SELECT count(*) FROM message_capture JOIN messages USING(cid,mid) WHERE occurred_at>=? AND occurred_at<?',args).fetchone()[0]
        blobs = db.execute('''SELECT DISTINCT a.sha256,a.local_path,a.size_bytes FROM attachments a JOIN messages m USING(cid,mid)
            WHERE m.occurred_at>=? AND m.occurred_at<? AND a.status='downloaded' ''',args)
        binary = Counter()
        for sha,rel,size in blobs:
            if not re.fullmatch('[a-f0-9]{64}',sha or '') or rel != 'attachments/'+sha:
                binary['invalid_locator'] += 1
                continue
            path = root/rel
            try:
                if path.is_symlink() or path.stat().st_size != size:
                    binary['invalid_size_or_link'] += 1
                    continue
                with path.open('rb') as f:
                    head = f.read(16); digest = hashlib.sha256(head)
                    for block in iter(lambda:f.read(1<<20),b''): digest.update(block)
                if digest.hexdigest() != sha:
                    binary['hash_mismatch'] += 1
                elif head.startswith((b'\x89PNG\r\n\x1a\n',b'\xff\xd8\xff',b'GIF87a',b'GIF89a',b'BM')) or head[:4]==b'RIFF' and head[8:12]==b'WEBP':
                    binary['verified_image'] += 1
                else: binary['verified_other'] += 1
            except OSError:
                binary['unreadable'] += 1
        return {'windowMs':args,'fullCycleMs':state.get('last_success_ms'),'messages':counts[0],'conversations':counts[1],
                'messagesWithoutCapture':counts[0]-captured,'detailReceipts':dict(by_window('message_detail_receipts','a.status,count(*)')),
                'relations':dict(by_window('message_relations','a.kind,count(*)')),
                'attachmentStates':dict(by_window('attachments',"a.status||':'||coalesce(a.extraction_status,a.failure_reason,'pending'),count(*)")),
                'occurrences':dict(by_window('attachment_occurrences',"coalesce(a.reason,'linked'),count(*)")),
                'uniqueBlobs':dict(binary),'recentDocumentOriginals':db.execute('SELECT count(*) FROM recent_document_content WHERE modified_ms BETWEEN ? AND ?',args).fetchone()[0]}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', required=True, type=Path)
    print(json.dumps(verify(parser.parse_args().root),ensure_ascii=False,indent=2))
