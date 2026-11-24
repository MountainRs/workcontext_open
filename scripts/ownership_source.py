"""Build bounded, account-scoped conversation inputs for the desktop review worker."""
import json
import os
import re
import sqlite3
import tempfile
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing

import archive_sync as archive
from capture_details import hydrate_page

DAY = 24 * archive.HOUR_MS
CREDENTIAL = re.compile(r'密钥|秘钥|口令|令牌|密码|验证码|授权码|访问码|提取码|助记词|私钥|API[ _-]?key|app[ _-]?secret|password|authorization\s*[:=]|bearer\s+|refresh.?token|access.?token|client.?secret|private key|BEGIN [A-Z ]*KEY|sk-[a-z0-9]|cookie\s*[:=]|ssh-(rsa|ed25519)|otpauth:', re.I)
IDENTIFIER = re.compile(r'(?:https?://|www\.|ssh://|dingtalk://|file://)\S+|[\w.+-]+@[\w.-]+\.[a-z]{2,}|[a-z0-9_+/=.:@!#$%-]{20,}|(?<!\d)\d{5,}(?!\d)', re.I)
OPAQUE = re.compile(r'[\W\da-zA-Z_]{3,}')
MACHINE = re.compile(r'```|-----BEGIN |^\s*(?:curl |export |set |[{}]|\w+\s*=)|<\?xml', re.I | re.M)


def clean(text, hidden=False):
    if hidden or CREDENTIAL.search(text) or MACHINE.search(text) or OPAQUE.fullmatch(text.strip()):
        return '[技术标识或访问材料已省略]'
    text = IDENTIFIER.sub('[标识]', text)
    return text[:1600] + (' [长文节选]' if len(text) > 1600 else '')


def read_bound(root, dws, expected_profile):
    uri = (root / 'archive' / 'archive.sqlite3').resolve().as_uri() + '?mode=ro'
    with closing(sqlite3.connect(uri, uri=True)) as db:
        if archive.state(db, 'profile') != expected_profile:
            raise ValueError('归档账号与授权账号不一致')
        profile = archive.bind_profile(db, dws)
        if profile != expected_profile:
            raise ValueError('当前 DWS 账号与授权账号不一致')
        alias = archive.state(db, 'self_open_id')
        # Revalidate the sender alias without modifying the live archive.
        with closing(sqlite3.connect(':memory:')) as scratch:
            scratch.execute('create table archive_state(key text primary key,value text)')
            scratch.execute('create table messages(sender_id text)')
            archive.bind_self(scratch, dws, profile)
            if not alias or archive.state(scratch, 'self_open_id') != alias:
                raise ValueError('归档本人身份与 DWS 不一致')
    return uri, profile, alias


def fetch_window(dws, profile, start, end, bounds):
    cursor = '0'
    seen = set()
    messages = {}
    with closing(sqlite3.connect(':memory:')) as receipts:
        while cursor is not None:
            if cursor in seen:
                raise ValueError('消息游标重复')
            seen.add(cursor)
            response = archive.dws_json(dws, 'chat', 'message', 'list-all', '--start', archive.dws_time(start), '--end', archive.dws_time(end), '--limit', '100', '--no-reactions', '--cursor', cursor, profile=profile)
            groups, cursor = archive.message_page(response)
            groups = list(groups)
            for group in groups:
                for msg in group['messages']:
                    msg.pop('_verified_detail', None)
            hydrate_page(receipts, dws, profile, {'start_ms': bounds[0], 'end_ms': bounds[1]}, groups, archive.dws_json)
            for group in groups:
                for msg in group['messages']:
                    stamp = archive.occurred_ms(msg['createTime'])
                    if not start <= stamp < end:
                        continue
                    cid = archive.identifier(archive._conversation_id(msg, group['openConversationId']), 'cid')
                    mid = archive.identifier(archive._message_id(msg), 'mid')
                    detail = msg.get('_verified_detail', {})
                    safe = not (archive._unsafe_forward_preview(msg, bounds) or archive._unsafe_forward_preview(detail, bounds))
                    body = archive._capture_fields(detail, allow_preview=safe)[1] or archive._capture_fields(msg, allow_preview=safe)[1]
                    quote = detail.get('quotedMessage') or msg.get('quotedMessage')
                    reply = None
                    if isinstance(quote, dict) and archive._conversation_id(quote, cid) == cid:
                        reply = archive._message_id(quote)
                    messages[cid, mid] = (stamp, msg.get('senderId'), body, reply, group.get('title'), group.get('singleChat'), False, msg.get('sender'))
    return messages


def write_json(path, value):
    with tempfile.NamedTemporaryFile('w', encoding='utf-8', dir=path.parent, delete=False) as f:
        name = f.name
        try:
            json.dump(value, f, ensure_ascii=False, separators=(',', ':'))
            f.close()
            os.replace(name, path)
        finally:
            if os.path.exists(name):
                f.close()
                os.unlink(name)


def collect(root, dws, start, end, uri, profile, alias):
    at_me = archive.dws_json(dws, 'chat', '+at-me', '--days', '30', '--page-all', '--no-reactions', profile=profile)
    archive.reject_partial(at_me)
    if not at_me.get('complete') or at_me.get('hasMore'):
        raise ValueError('本人提及索引未读完')
    mentions = {(r['conversationId'], r['messageId']) for r in archive.rows(at_me, 'messages')}
    by_key = {}
    windows = range(start, end, DAY)
    with ThreadPoolExecutor(max_workers=4) as pool:
        for result in pool.map(lambda s: fetch_window(dws, profile, s, min(s + DAY, end), (start, end)), windows):
            by_key.update(result)
    user = profile.split(':', 1)[1]
    with closing(sqlite3.connect(uri, uri=True)) as db:
        db.execute('BEGIN')
        if archive.state(db, 'profile') != profile:
            raise ValueError('归档账号已变更')
        sql = '''SELECT m.cid,mid,occurred_at,sender_id,body,reply_to,title,single_chat,mentions_json,sender_name
                 FROM messages m JOIN conversations USING(cid) WHERE occurred_at>=? AND occurred_at<?'''
        for cid, mid, stamp, sender, body, reply, title, single, ats, name in db.execute(sql, (start, end)):
            key = (cid, mid)
            if user in json.loads(ats or '[]'):
                mentions.add(key)
            by_key.setdefault(key, (stamp, sender, body, reply, title, single, False, name))
    cache_path = root / 'ownership' / 'sources.json'
    if cache_path.exists():
        cached = json.loads(cache_path.read_text(encoding='utf-8'))
        if cached['profile'] == profile:
            for cid, mid, record in cached['records']:
                if start <= record[0] < end:
                    by_key.setdefault((cid, mid), record)
                    if record[6]:
                        mentions.add((cid, mid))
    retained = []
    groups = defaultdict(list)
    for (cid, mid), (stamp, sender, body, reply, title, single, _, name) in by_key.items():
        groups[cid].append((stamp, mid, sender, body or '', reply, title, single, name))
    conversations = {}
    locations = {}
    for cid, group in groups.items():
        group.sort(key=lambda r: (r[0], r[1]))
        ids = {r[1]: i for i, r in enumerate(group, 1)}
        actors = {alias: '我', user: '我'}
        names = defaultdict(set)
        masked = set()
        for i, (stamp, _, sender, text, _, _, _, name) in enumerate(group):
            if sender:
                actors.setdefault(sender, '成员' + str(len(actors) - 1))
                if name:
                    names[name].add(actors[sender])
            if CREDENTIAL.search(text):
                masked.update(n for n in range(max(0, i - 2), min(len(group), i + 3)) if abs(group[n][0] - stamp) <= 600_000)
        rows = []
        for i, (stamp, mid, sender, text, reply, title, single, _) in enumerate(group, 1):
            excerpt = clean(text, i - 1 in masked) or '[无可读正文]'
            retained.append((cid, mid, (stamp, sender, excerpt, reply, title, single, (cid, mid) in mentions, group[i - 1][7])))
            for name in sorted(names, key=len, reverse=True):
                excerpt = excerpt.replace(name, next(iter(names[name])) if len(names[name]) == 1 else '同名成员')
            time = archive.dws_time(stamp)
            actor = actors.get(sender, '未知')
            rows.append([i, time, actor, ['我'] if (cid, mid) in mentions else [], ids.get(reply, '窗口外' if reply else ''), excerpt])
            parent = ids.get(reply)
            addressed = bool(sender and actor != '我' and (single or (cid, mid) in mentions or
                             (parent and group[parent - 1][2] in (user, alias))))
            kind = '私聊' if single == 1 else '群聊' if single == 0 else ''
            locations[cid, i] = {'cid': cid, 'mid': mid, 'time': time, 'kind': kind, 'conversation': title or kind,
                                 'excerpt': f'{actor}：{excerpt[:430]}', 'fromMe': actor == '我', 'toMe': addressed}
        conversations[cid] = {'type': '私聊' if group[-1][6] else '群聊', 'cutoff': archive.dws_time(end), 'messages': rows}
    write_json(cache_path, {'profile': profile, 'records': retained})
    return conversations, locations
