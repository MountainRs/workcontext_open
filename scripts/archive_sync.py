"""Archive the current DWS account without emitting message contents."""

import argparse
import importlib
import json
import os
import re
import sqlite3
import subprocess
import sys
import time
from contextlib import closing, contextmanager
from datetime import datetime, timedelta, timezone
from pathlib import Path


HOUR_MS = 3_600_000
INITIAL_MS = 168 * HOUR_MS
OVERLAP_MS = 6 * HOUR_MS
PAGE_SIZE = 50
# DWS sends list-all boundaries and renders naive message times in Asia/Shanghai.
DWS_TZ = timezone(timedelta(hours=8))
EPOCH = datetime(1970, 1, 1, tzinfo=timezone.utc)

SCHEMA = """
CREATE TABLE IF NOT EXISTS conversations (
    cid TEXT NOT NULL PRIMARY KEY,
    title TEXT,
    conversation_type TEXT,
    single_chat INTEGER
);
CREATE TABLE IF NOT EXISTS messages (
    cid TEXT NOT NULL REFERENCES conversations(cid),
    mid TEXT NOT NULL,
    body TEXT,
    sender_id TEXT,
    sender_name TEXT,
    occurred_at INTEGER NOT NULL,
    reply_to TEXT,
    quoted_json TEXT,
    PRIMARY KEY (cid, mid)
);
CREATE INDEX IF NOT EXISTS messages_occurred_at ON messages(occurred_at);
CREATE TABLE IF NOT EXISTS message_capture (
    cid TEXT NOT NULL, mid TEXT NOT NULL, message_type TEXT, raw_json TEXT NOT NULL, thread_id TEXT,
    PRIMARY KEY(cid,mid), FOREIGN KEY(cid,mid) REFERENCES messages(cid,mid) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS message_relations (
    cid TEXT NOT NULL, mid TEXT NOT NULL, path TEXT NOT NULL, kind TEXT NOT NULL,
    source_cid TEXT NOT NULL, source_mid TEXT NOT NULL, occurred_at INTEGER,
    sender_id TEXT, sender_name TEXT, message_type TEXT, body TEXT, raw_json TEXT,
    PRIMARY KEY(cid,mid,path), FOREIGN KEY(cid,mid) REFERENCES messages(cid,mid) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS archive_state (
    key TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS archive_failures (
    occurred_at INTEGER NOT NULL,
    stage TEXT NOT NULL,
    error TEXT NOT NULL
);
"""

def init_change_feed(conn, bootstrap_limit=512):
    """Install transactional evidence revisions; seed one bounded legacy page per sync."""
    conn.execute('''CREATE TABLE IF NOT EXISTS organizer_changes (
        seq INTEGER PRIMARY KEY AUTOINCREMENT, cid TEXT NOT NULL, mid TEXT NOT NULL,
        UNIQUE(cid,mid))''')
    conn.execute('CREATE INDEX IF NOT EXISTS organizer_replies ON messages(cid,reply_to) WHERE reply_to IS NOT NULL')
    columns = {
        'messages': ('body', 'occurred_at', 'sender_id', 'sender_name', 'reply_to', 'quoted_json'),
        'message_capture': ('thread_id', 'message_type', 'raw_json'),
        'message_relations': ('kind', 'source_cid', 'source_mid', 'occurred_at', 'sender_id', 'body', 'raw_json'),
        'attachments': ('status', 'extraction_status', 'sha256', 'text_content', 'name'),
        'attachment_occurrences': ('attachment_id', 'source_cid', 'source_mid', 'path', 'reason'),
    }
    for table, fields in columns.items():
        changed = ' OR '.join(f'OLD.{field} IS NOT NEW.{field}' for field in fields)
        for action, row, condition in [('INSERT', 'NEW', ''), ('DELETE', 'OLD', ''),
                                       ('UPDATE', 'NEW', 'WHEN ' + changed)]:
            replies = ''
            if table in ('messages', 'message_capture', 'message_relations'):
                replies = f'''INSERT INTO organizer_changes(cid,mid) SELECT cid,mid FROM messages
                    WHERE cid={row}.cid AND reply_to={row}.mid AND mid<>{row}.mid
                    ON CONFLICT(cid,mid) DO UPDATE SET seq=excluded.seq;'''
            conn.execute(f'''CREATE TRIGGER IF NOT EXISTS organizer_{table}_{action}
                AFTER {action} ON {table} {condition} BEGIN
                INSERT INTO organizer_changes(cid,mid) VALUES({row}.cid,{row}.mid)
                 ON CONFLICT(cid,mid) DO UPDATE SET seq=excluded.seq;
                {replies}
                END''')

    # A late message can replace a previously admitted predecessor. Revise only the
    # immediate successor group and tied timestamps affected by insertion/deletion.
    conn.execute('CREATE INDEX IF NOT EXISTS organizer_time ON messages(cid,occurred_at)')
    for operation, rows in [('INSERT', ['NEW']), ('DELETE', ['OLD']), ('UPDATE', ['OLD', 'NEW'])]:
        periods = []
        for row in rows:
            periods.append(f"""SELECT m.cid,m.mid FROM messages m WHERE m.cid={row}.cid AND m.reply_to IS NULL
                AND m.mid<>{row}.mid AND m.occurred_at=(SELECT min(q.occurred_at) FROM messages q
                    WHERE q.cid={row}.cid AND q.occurred_at BETWEEN {row}.occurred_at+1 AND {row}.occurred_at+600000)""")
            # Editing text cannot change a same-timestamp tie.
            tie = ' AND OLD.occurred_at IS NOT NEW.occurred_at' if operation == 'UPDATE' else ''
            periods.append(f'SELECT cid,mid FROM messages WHERE cid={row}.cid AND occurred_at={row}.occurred_at AND mid<>{row}.mid AND reply_to IS NULL{tie}')
        when = ' WHEN OLD.body IS NOT NEW.body OR OLD.sender_name IS NOT NEW.sender_name OR OLD.sender_id IS NOT NEW.sender_id OR OLD.occurred_at IS NOT NEW.occurred_at' if operation == 'UPDATE' else ''
        conn.execute(f"""CREATE TRIGGER IF NOT EXISTS organizer_adjacent_{operation} AFTER {operation} ON messages{when} BEGIN
            INSERT INTO organizer_changes(cid,mid) SELECT cid,mid FROM ({' UNION '.join(periods)}) WHERE true
            ON CONFLICT(cid,mid) DO UPDATE SET seq=excluded.seq; END""")

    # Capturing an older body later, changing its thread, or identifying a forward
    # changes eligibility even when the message timestamp and text stay identical.
    for table, field in [('message_capture', 'thread_id'), ('message_relations', 'kind')]:
        for operation, row in [('INSERT', 'NEW'), ('DELETE', 'OLD'), ('UPDATE', 'NEW')]:
            when = f' WHEN OLD.{field} IS NOT NEW.{field}' if operation == 'UPDATE' else ''
            conn.execute(f"""CREATE TRIGGER IF NOT EXISTS organizer_adjacent_{table}_{operation}
                AFTER {operation} ON {table}{when} BEGIN
                INSERT INTO organizer_changes(cid,mid) SELECT m.cid,m.mid FROM messages m JOIN messages p ON p.cid=m.cid
                WHERE p.cid={row}.cid AND p.mid={row}.mid AND m.reply_to IS NULL AND m.occurred_at=(
                    SELECT min(q.occurred_at) FROM messages q WHERE q.cid=p.cid
                      AND q.occurred_at BETWEEN p.occurred_at+1 AND p.occurred_at+600000)
                ON CONFLICT(cid,mid) DO UPDATE SET seq=excluded.seq; END""")

    cursor = state(conn, 'organizer_bootstrap_rowid')
    if cursor == 'done':
        return
    rows = conn.execute('SELECT rowid,cid,mid FROM messages WHERE rowid>? ORDER BY rowid LIMIT ?',
                        (int(cursor or 0), bootstrap_limit)).fetchall()
    conn.executemany('INSERT INTO organizer_changes(cid,mid) VALUES(?,?) ON CONFLICT DO NOTHING',
                     ((cid, mid) for _, cid, mid in rows))
    set_state(conn, 'organizer_bootstrap_rowid', rows[-1][0] if len(rows) == bootstrap_limit else 'done')


UPSERT_CONVERSATION = """
INSERT INTO conversations(cid, title, conversation_type, single_chat)
VALUES (?, ?, ?, ?)
ON CONFLICT(cid) DO UPDATE SET
    title = COALESCE(excluded.title, conversations.title),
    conversation_type = COALESCE(excluded.conversation_type, conversations.conversation_type),
    single_chat = COALESCE(excluded.single_chat, conversations.single_chat)
"""

UPSERT_MESSAGE = """
INSERT INTO messages(cid, mid, body, sender_id, sender_name, occurred_at, reply_to, quoted_json)
VALUES (?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(cid, mid) DO UPDATE SET
    body = excluded.body, sender_id = excluded.sender_id, sender_name = excluded.sender_name,
    occurred_at = excluded.occurred_at, reply_to = excluded.reply_to, quoted_json = excluded.quoted_json
"""


class SyncError(Exception):
    """Diagnostics constructed locally, never interpolated from message payloads."""


@contextmanager
def writer_lock(root):
    # SQLite readers can see committed pages while DWS and the media worker run.
    with (root / ".sync.lock").open("ab") as handle:
        try:
            if os.name == "nt":
                import msvcrt

                handle.seek(0)
                msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
            else:
                import fcntl

                fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            raise SyncError("cannot lock archive; another sync may be running") from None
        yield


def now_ms():
    return time.time_ns() // 1_000_000


def state(conn, key):
    row = conn.execute("SELECT value FROM archive_state WHERE key = ?", (key,)).fetchone()
    return row[0] if row else None


def set_state(conn, key, value):
    conn.execute(
        "INSERT INTO archive_state VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (key, str(value)),
    )


def dws_json(dws, *args, profile=None, cwd=None):
    command = [dws, *args, "--format", "json"]
    if profile is not None:
        command.extend(("--profile", profile))
    try:
        run = subprocess.run(command, stdin=subprocess.DEVNULL, capture_output=True, timeout=90, cwd=cwd,
                             creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
    except subprocess.TimeoutExpired:
        raise SyncError("DWS request timed out") from None
    except OSError:
        raise SyncError("could not launch DWS") from None
    if run.returncode:
        raise SyncError(f"DWS exited with code {run.returncode}")
    try:
        payload = json.loads(run.stdout)
    except (ValueError, UnicodeError):
        raise SyncError("DWS returned invalid JSON") from None
    payload = obj(payload, "DWS result")
    if payload.get("success") is False or payload.get("error") or (
        "outcome" in payload and payload["outcome"] != "success"
    ):
        raise SyncError("DWS reported failure or a partial result")
    return payload


def obj(value, field):
    if not isinstance(value, dict):
        raise SyncError(f"expected object: {field}")
    return value


def rows(data, key):
    items = data.get(key)
    if not isinstance(items, list):
        raise SyncError(f"expected array: {key}")
    for item in items:
        yield obj(item, key)


def identifier(value, field):
    if not isinstance(value, str) or not value.strip():
        raise SyncError(f"missing or invalid {field}")
    return value


def text(value):
    return value if isinstance(value, str) else None


def bind_profile(conn, dws, expected_profile=None):
    data = dws_json(dws, "profile", "list")
    current = identifier(data.get("currentProfile"), "currentProfile")
    profiles = list(rows(data, "profiles"))
    matches = [row for row in profiles if row.get("isCurrent") is True]
    if len(matches) != 1 or matches[0].get("profile") != current:
        raise SyncError("DWS has no unique current profile")
    account = matches[0]
    corp = identifier(account.get("corpId"), "corpId")
    user = identifier(account.get("userId"), "userId")
    org_current = [row for row in profiles if row.get("corpId") == corp and row.get("isOrgCurrent") is True]
    if current != f"{corp}:{user}" or org_current != [account]:
        raise SyncError("DWS current profile is not a verified account selector")
    with conn:
        bound = state(conn, "profile")
        if expected_profile is not None and (current != expected_profile or bound != expected_profile):
            raise SyncError("scheduled capture requires matching archive and current DWS profiles")
        if bound is None:
            if conn.execute(
                "SELECT EXISTS (SELECT 1 FROM conversations) OR EXISTS (SELECT 1 FROM messages)"
            ).fetchone()[0]:
                raise SyncError("refusing to bind a nonempty archive without a profile")
            set_state(conn, "profile", current)
        elif bound != current:
            raise SyncError("current DWS profile differs from the archive's bound profile")
    return current


def bind_self(conn, dws, profile):
    corp, user = profile.split(":", 1)
    payload = dws_json(dws, "contact", "user", "get-self", profile=profile)
    data = obj(payload.get("data", payload), "self result")
    verified = set()
    for row in rows(data, "result"):
        employee = obj(row.get("orgEmployeeModel"), "self employee")
        if (employee.get("userId") == user and row.get("corpId", corp) == corp
                and employee.get("corpId", corp) == corp):
            verified.add(identifier(employee.get("openDingTalkId"), "self openDingTalkId"))
    if len(verified) != 1:
        raise SyncError("DWS has no unique verified sender identity in the bound profile")
    alias = verified.pop()
    with conn:
        set_state(conn, "self_open_id", alias)
        if user != alias:
            conn.execute("UPDATE messages SET sender_id=? WHERE sender_id=?", (user, alias))


def reject_partial(data):
    if (data.get("partial") or data.get("truncated") or data.get("failures")
            or data.get("failedCount") or data.get("paginationKnown") is False):
        raise SyncError("DWS reported incomplete pagination")


def fixed_window(conn, invocation_ms, *, capture=False, checkpoint="message_window"):
    # DWS accepts whole seconds: round the lower bound inward, not outward.
    earliest = (invocation_ms - INITIAL_MS + 999) // 1000 * 1000
    end_now = invocation_ms // 1000 * 1000
    saved = state(conn, checkpoint)
    if saved is not None:
        window = json.loads(saved)
        start, end, cursor = window["start_ms"], window["end_ms"], window["cursor"]
        if not (type(start) is int and type(end) is int and 0 <= start < end
                and start % 1000 == end % 1000 == 0
                and (cursor is None or isinstance(cursor, str) and cursor.strip())):
            raise SyncError("invalid saved message window")
        if end > end_now:
            raise SyncError("clock precedes saved message window")
        if start >= earliest:
            return window
        # An opaque cursor belongs to its original window. Re-read only the
        # authorized interval; page upserts keep already committed rows stable.
    end = end_now
    last = state(conn, "last_success_ms")
    if last is None:
        start = earliest
    else:
        last_ms = int(last)
        if not 0 < last_ms <= end:
            raise SyncError("clock precedes last successful message window")
        start = max(earliest, (last_ms - OVERLAP_MS) // 1000 * 1000)
    if capture:
        start = earliest
    window = {"start_ms": start, "end_ms": end, "cursor": "0"}
    if capture:
        window["capture_v1"] = True
    with conn:
        set_state(conn, checkpoint, json.dumps(window))
    return window


def dws_time(ms):
    return (EPOCH + timedelta(milliseconds=ms)).astimezone(DWS_TZ).strftime("%Y-%m-%d %H:%M:%S")


def occurred_ms(value):
    if type(value) is int and 0 <= value < 2**63:
        return value
    if isinstance(value, str):
        try:
            stamp = datetime.fromisoformat(value[:-1] + "+00:00" if value.endswith("Z") else value)
            if stamp.tzinfo is None:
                stamp = stamp.replace(tzinfo=DWS_TZ)
            return (stamp - EPOCH) // timedelta(milliseconds=1)
        except (ValueError, OverflowError):
            pass
    raise SyncError("missing or invalid message createTime")


def message_page(payload):
    data = obj(payload.get("data", payload), "message result")
    result = obj(data.get("result", data), "message result")
    reject_partial(data)
    reject_partial(result)
    if data.get("paging"):
        reject_partial(obj(data["paging"], "message paging"))
    more = result.get("hasMore")
    if more is True:
        token = result.get("nextCursor")
        cursor = str(token) if type(token) is int and token > 0 else identifier(token, "message nextCursor")
        if cursor.strip().lower() in ("0", "null", "<nil>"):
            raise SyncError("invalid DWS message continuation cursor")
    elif more is False:
        if data.get("complete") is False or result.get("complete") is False:
            raise SyncError("DWS ended pagination without confirming completion")
        cursor = None
    else:
        raise SyncError("DWS did not return message pagination status")
    meta = obj(payload.get("meta", {}), "message meta")
    if "pagination" in meta:
        page = obj(meta["pagination"], "message pagination")
        if page.get("endpoint_exhausted") is not (not more) or page.get("next_token") != cursor:
            raise SyncError("conflicting DWS message pagination status")
    return rows(result, "conversationMessagesList"), cursor


_RELATIONS = {"quotedMessage": "quote", "forwarded": "forward", "forward_messages": "forward"}
# An opaque resource URL may expire even without a signature in its query.
# Keep stable resource IDs in the media ledger; never persist URL credentials.
_URL = re.compile(r"(?:https?:)?//[^\s<>\"'，。；）]+", re.IGNORECASE)
_DOC_NODE = re.compile(r"https://alidocs\.dingtalk\.com/i/nodes/[A-Za-z0-9_-]+(?=[?#)/]|$)")


def _safe_link(match):
    node = _DOC_NODE.match(match.group())
    return node.group() if node else "[链接已省略]"
_TEXT_KEYS = ("textContent", "text", "richText", "markdown", "content", "title", "description")


def _structured(value):
    if isinstance(value, str) and value.lstrip().startswith(("{", "[")):
        try:
            return json.loads(value)
        except ValueError:
            pass  # Plain text can start with a bracket (e.g. an image placeholder).
    return value


def _safe_body(value):
    """Retain native vs serialized representation, except for unsafe material."""
    if isinstance(value, dict):
        return {k: _safe_body(v) for k, v in value.items() if not _URL.search(k)
                and k not in _RELATIONS and k not in ("messages", "quoted_message", "_verified_detail")
                and not k.lower().endswith(("url", "token", "signature"))}
    if isinstance(value, list):
        return [_safe_body(v) for v in value]
    if isinstance(value, str):
        decoded = _structured(value)
        if isinstance(decoded, (dict, list)):
            safe = _safe_body(decoded)
            return value if safe == decoded else json.dumps(safe, ensure_ascii=False)
        return _URL.sub(_safe_link, value)
    return value


def _readable(value):
    value = _structured(value)
    if isinstance(value, str):
        return value if value.strip() else None
    if isinstance(value, list):
        return "\n".join(filter(None, map(_readable, value))) or None
    if isinstance(value, dict):
        # Use semantic text fields, not IDs, filenames or serialized JSON as speech.
        parts = dict.fromkeys(part for key in _TEXT_KEYS if key in value
                              if (part := _readable(value[key])))
        return "\n".join(parts) or None
    return None


def _child_time(child):
    try:
        return occurred_ms(child.get("createTime"))
    except SyncError:
        return None


def _inside(stamp, authorized):
    return stamp is not None and authorized[0] <= stamp < authorized[1]


def _children(message):
    for key, kind in _RELATIONS.items():
        children = message.get(key)
        if isinstance(children, dict):
            yield key, kind, children
        elif isinstance(children, list):
            for i, child in enumerate(children):
                if isinstance(child, dict):
                    yield f"{key}[{i}]", kind, child


def _unsafe_forward_preview(message, authorized):
    # A combined-forward display string can itself be a copy of old messages.
    # Unlike the sender's own quote reply, this preview isn't attributable text.
    return any(kind == "forward" and (not _inside(_child_time(child), authorized)
                                      or _unsafe_forward_preview(child, authorized))
               for _, kind, child in _children(message))


def _capture_fields(message, *, allow_preview):
    if not allow_preview:
        return {}, None
    raw = {key: _safe_body(message[key]) for key in ("text", "content") if key in message}
    return raw, _readable(raw.get("text")) or _readable(raw.get("content"))


def _message_id(message):
    return text(message.get("messageId") or message.get("openMessageId") or message.get("message_id"))


def _conversation_id(message, inherited):
    return message.get("conversationId") or message.get("openConversationId") or inherited


def _message_type(message):
    unknown = None
    for key in ("messageType", "contentType", "sendType"):
        value = message.get(key)
        if type(value) in (str, int) and str(value).strip():
            if str(value).lower() != "unknown":
                return str(value)
            unknown = str(value)
    for key in ("text", "content"):
        part = _structured(message.get(key))
        if isinstance(part, dict) and type(part.get("contentType")) in (str, int):
            return str(part["contentType"])
    return unknown


def _relations(message, cid, authorized, self_open_id, user_id, prefix=""):
    for key, kind, child in _children(message):
        source_cid, source_mid = _conversation_id(child, cid), _message_id(child)
        if not isinstance(source_cid, str) or not source_cid.strip() or not source_mid or not source_mid.strip():
            continue
        path = prefix + key
        stamp = _child_time(child)
        allowed = _inside(stamp, authorized)
        raw, body = (_capture_fields(child, allow_preview=not _unsafe_forward_preview(child, authorized))
                     if allowed else ({}, None))
        sender = text(child.get("senderId") or child.get("senderOpenDingTalkId"))
        if self_open_id is not None and sender == self_open_id:
            sender = user_id
        yield (path, kind, source_cid, source_mid, stamp, sender, text(child.get("sender")),
               _message_type(child), body, json.dumps(raw, ensure_ascii=False) if raw else None)
        if allowed:
            yield from _relations(child, source_cid, authorized, self_open_id, user_id, path + ".")


def save_messages(conn, groups, window, media, self_open_id, user_id, *, authorized=None):
    authorized = authorized or (window["start_ms"], window["end_ms"])
    for group in groups:
        group_cid = identifier(group.get("openConversationId"), "group openConversationId")
        single = group.get("singleChat")
        saved_group = False
        for message in rows(group, "messages"):
            stamp = occurred_ms(message.get("createTime"))
            if not window["start_ms"] <= stamp < window["end_ms"]:
                continue
            if not saved_group:
                conn.execute(UPSERT_CONVERSATION, (
                    group_cid, text(group.get("title")), None, single if type(single) is bool else None,
                ))
                saved_group = True
            cid = identifier(_conversation_id(message, group_cid), "message conversationId")
            mid = identifier(_message_id(message), "messageId")
            if cid != group_cid:
                conn.execute(UPSERT_CONVERSATION, (
                    cid, text(message.get("conversationTitle")), None, None,
                ))
            detail = message.get("_verified_detail")
            if detail is not None and (not isinstance(detail, dict) or _message_id(detail) != mid
                    or _conversation_id(detail, cid) != cid or not _inside(_child_time(detail), authorized)):
                raise SyncError("detail message identity or time mismatch")
            allow_preview = not (_unsafe_forward_preview(message, authorized)
                                 or detail and _unsafe_forward_preview(detail, authorized))
            raw, body = _capture_fields(message, allow_preview=allow_preview)
            raw["createTime"] = message["createTime"]
            if text(message.get("openMessageId")):
                raw["openMessageId"] = message["openMessageId"]
            rels = list(_relations(message, cid, authorized, self_open_id, user_id))
            if detail is not None:
                detail_raw, detail_body = _capture_fields(detail, allow_preview=allow_preview)
                detail_raw["createTime"] = detail["createTime"]
                raw["detail"] = detail_raw
                body = detail_body or body
                rels.extend(_relations(detail, cid, authorized, self_open_id, user_id, "detail."))
            quotes = {(r[2], r[3]) for r in rels if r[0] in ("quotedMessage", "detail.quotedMessage")}
            quote = next(iter(quotes)) if len(quotes) == 1 else None
            reply = quote[1] if quote and quote[0] == cid else None
            quoted = {"conversationId": quote[0], "messageId": quote[1]} if quote else None
            sender = text(message.get("senderId"))
            if self_open_id is not None and sender == self_open_id:
                sender = user_id
            conn.execute(UPSERT_MESSAGE, (
                cid, mid, body, sender, text(message.get("sender")),
                stamp, reply, json.dumps(quoted, ensure_ascii=False) if quoted is not None else None,
            ))
            conn.execute("""INSERT INTO message_capture(cid,mid,message_type,raw_json,thread_id)
                VALUES(?,?,?,?,?) ON CONFLICT(cid,mid) DO UPDATE SET message_type=excluded.message_type,
                raw_json=excluded.raw_json,thread_id=excluded.thread_id""", (
                cid, mid, _message_type(detail or {}) or _message_type(message), json.dumps(raw, ensure_ascii=False),
                text(message.get("threadId")) or text((detail or {}).get("threadId")),
            ))
            old_rels = conn.execute('''SELECT path,kind,source_cid,source_mid,occurred_at,sender_id,
                sender_name,message_type,body,raw_json FROM message_relations WHERE cid=? AND mid=?
                ORDER BY path''', (cid, mid)).fetchall()
            if old_rels != sorted(rels, key=lambda r: r[0]):
                conn.execute("DELETE FROM message_relations WHERE cid=? AND mid=?", (cid, mid))
                conn.executemany("""INSERT INTO message_relations(cid,mid,path,kind,source_cid,source_mid,
                    occurred_at,sender_id,sender_name,message_type,body,raw_json) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)""",
                    ((cid, mid, *r) for r in rels))
            media.enqueue(conn, cid, mid, message)


def sync_messages(conn, dws, profile, window, media, *, checkpoint="message_window", authorized=None):
    from capture_details import hydrate_page
    self_open_id, user_id = state(conn, "self_open_id"), profile.split(":", 1)[1]
    args = ("chat", "message", "list-all", "--start", dws_time(window["start_ms"]),
            "--end", dws_time(window["end_ms"]), "--limit", str(PAGE_SIZE), "--no-reactions")
    seen = set()
    while window["cursor"] is not None:
        cursor = window["cursor"]
        seen.add(cursor)
        payload = dws_json(dws, *args, "--cursor", cursor, profile=profile)
        groups, next_cursor = message_page(payload)
        if next_cursor in seen:
            raise SyncError("repeated DWS message cursor")
        # A crash commits both the page and its media jobs, or neither of them.
        with conn:
            groups = list(groups)
            for group in groups:
                for entry in rows(group, "messages"):
                    entry.pop("_verified_detail", None)  # Private, never trusted from a list response.
            hydrate_page(conn, dws, profile, window, groups, dws_json)
            save_messages(conn, groups, window, media, self_open_id, user_id, authorized=authorized)
            window["cursor"] = next_cursor
            set_state(conn, checkpoint, json.dumps(window))
        if next_cursor is not None:
            time.sleep(0.2)


def enrich_mentions(conn, dws, profile):
    data = dws_json(dws, "chat", "+at-me", "--days", "7", "--limit", "30",
                    "--page-all", "--no-reactions", profile=profile)
    reject_partial(data)
    if not data.get("complete") or data.get("hasMore"):
        raise SyncError("DWS did not exhaust the @me index")
    account = json.dumps([profile.split(":", 1)[1]])
    with conn:
        if not any(row[1] == "mentions_json" for row in conn.execute("PRAGMA table_info(messages)")):
            conn.execute("ALTER TABLE messages ADD COLUMN mentions_json TEXT")
        conn.executemany("UPDATE messages SET mentions_json=? WHERE cid=? AND mid=?", (
            (account, identifier(m.get("conversationId"), "mention conversationId"),
             identifier(m.get("messageId"), "mention messageId")) for m in rows(data, "messages")
        ))


def sync(conn, root, dws, invocation_ms, expected_profile=None):
    stage = "profile"
    try:
        profile = bind_profile(conn, dws, expected_profile)
        stage = "self-identity"
        bind_self(conn, dws, profile)
        stage = "window"
        needs_capture = state(conn, "message_capture_version") != "1"
        window = fixed_window(conn, invocation_ms, capture=needs_capture)
        authorized = (invocation_ms - INITIAL_MS, invocation_ms)
        stage = "media-init"
        try:
            media = importlib.import_module("archive_media")
        except ModuleNotFoundError as exc:
            if exc.name != "archive_media":
                raise
            raise SyncError("archive_media is not installed; fixed window prepared") from None
        with conn:
            media.init(conn)
            init_change_feed(conn)
        stage = "messages"
        if needs_capture and not window.get("capture_v1"):
            # A legacy cursor may already have passed pages that lack the new
            # representation. Replay separately; its normal checkpoint stays valid.
            checkpoint = "message_capture_window"
            replay = fixed_window(conn, invocation_ms, capture=True, checkpoint=checkpoint)
            sync_messages(conn, dws, profile, replay, media, checkpoint=checkpoint, authorized=authorized)
            with conn:
                set_state(conn, "message_capture_version", 1)
                conn.execute("DELETE FROM archive_state WHERE key=?", (checkpoint,))
        sync_messages(conn, dws, profile, window, media, authorized=authorized)
        if needs_capture and window.get("capture_v1"):
            with conn:
                set_state(conn, "message_capture_version", 1)
                conn.execute("DELETE FROM archive_state WHERE key='message_capture_window'")
        stage = "mentions"
        enrich_mentions(conn, dws, profile)
        # Message-only consumers need not wait for all attachment downloads.
        # Keep the full-cycle watermark unchanged until the drain returns.
        with conn:
            set_state(conn, "messages_ready_ms", window["end_ms"])
        stage = "media-drain"
        media.drain(conn, root, dws, profile, invocation_ms - INITIAL_MS, invocation_ms)
        stage = "finish"
        with conn:
            set_state(conn, "last_success_ms", window["end_ms"])
            set_state(conn, "error", "")
            conn.execute("DELETE FROM archive_state WHERE key = 'message_window'")
    except (Exception, KeyboardInterrupt) as exc:
        # Do not publish raw DWS output, SQLite parameters, or media exception text.
        detail = str(exc) if isinstance(exc, SyncError) else type(exc).__name__
        error = f"{stage}: {detail}"
        conn.rollback()
        with conn:
            set_state(conn, "error", error)
            conn.execute("INSERT INTO archive_failures VALUES (?, ?, ?)", (now_ms(), stage, error))
        raise SyncError(error) from None


def sync_documents(conn, root, dws, invocation_ms):
    from archive_documents import inspect_recent
    profile = state(conn, "profile")
    if not profile:
        raise SyncError("document check requires a bound profile")
    def call(*args, **kwargs):
        try:
            return dws_json(dws, *args, profile=profile, **kwargs)
        except SyncError as exc:
            raise RuntimeError(str(exc)) from None
    try:
        inspect_recent(conn, root, call, invocation_ms)
        status = "已检查近期编辑索引"
    except (KeyError, TypeError, ValueError, RuntimeError, OSError):
        status = "近期编辑索引核验未完成"
    with conn:
        set_state(conn, "document_status", status)
        if status == "已检查近期编辑索引":
            set_state(conn, "documents_ready_ms", invocation_ms)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--dws", required=True, metavar="ABS")
    parser.add_argument("--expected-profile", help="require this existing archive binding and current DWS profile")
    args = parser.parse_args()
    if not os.path.isabs(args.dws):
        parser.error("--dws must be an absolute path")
    invocation_ms = now_ms()
    os.umask(0o077)
    try:
        root = args.root.resolve()
        root.mkdir(parents=True, exist_ok=True)
        with writer_lock(root), closing(sqlite3.connect(root / "archive.sqlite3")) as conn:
            conn.execute("PRAGMA foreign_keys = ON")
            conn.executescript(SCHEMA)
            sync(conn, root, args.dws, invocation_ms, args.expected_profile)
            sync_documents(conn, root, args.dws, invocation_ms)
    except (Exception, KeyboardInterrupt) as exc:
        detail = str(exc) if isinstance(exc, SyncError) else type(exc).__name__
        print(f"archive sync: {detail}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
