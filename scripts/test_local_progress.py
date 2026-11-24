"""Exercise the exact desktop statement without a DWS account."""
import sqlite3
import unittest
from pathlib import Path

from archive_sync import SCHEMA
from archive_media import init

SQL = (Path(__file__).resolve().parents[1] / 'crates/desktop/src/local_progress.sql').read_text(encoding='utf-8')


class LocalProgressTest(unittest.TestCase):
    def test_only_opposite_party_same_thread_in_authorized_window(self):
        with sqlite3.connect(':memory:') as db:
            db.executescript(SCHEMA)
            db.execute('ALTER TABLE messages ADD COLUMN mentions_json TEXT')
            init(db)
            db.executemany('INSERT INTO conversations(cid,single_chat) VALUES (?,?)', [('dm', 1), ('other', 1), ('group', 0)])
            db.executemany('INSERT INTO messages(cid,mid,sender_id,body,occurred_at,reply_to,mentions_json) VALUES (?,?,?,?,?,?,?)', [
                ('dm','task','me','请确认版本',100,None,None),
                ('other','wrong','them','已通过',101,'task',None),
                ('dm','self','me','已通过',102,'task',None),
                ('dm','late','them','已通过',201,'task',None),
                ('dm','noquote','them','已通过',103,None,None),
                ('group','task','them','请确认版本',110,None,'["me"]'),
                ('group','unknown','third','已通过',111,'task',None),
                ('group','answer','me','哪个版本？',112,'task',None),
                ('group','ignored','them','请确认数量',110,None,'["third"]'),
            ])
            rows = db.execute(SQL, {'me':'me','since':100,'until':200}).fetchall()
            self.assertEqual([(r[9],r[10],r[11],r[4]) for r in rows], [('group','task','answer','哪个版本？'),('dm','task',None,None)])


if __name__ == '__main__':
    unittest.main()
