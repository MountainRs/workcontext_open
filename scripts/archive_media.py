"""DWS message attachment queue for an archive.sqlite3 connection.

Call init/enqueue in the collector's transaction. Call drain after committing it
for per-item durability; drain never commits an existing caller transaction.
Use the same stable DWS profile for collection and draining of a private archive.
local_path is relative to root; name is display metadata, never a path. Download
and extraction have separate status/error fields; extracted text is not logged.
Files are capped at 64 MiB, text at 256 KiB per attachment, and PDF at 256 pages.
OOXML needs Python alone; see archive_media_extract.py for optional legacy
Office, PDF and OCR readers.
"""

import errno
import hashlib
import json
import os
from contextlib import contextmanager, nullcontext
from pathlib import Path
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time


_STORE = "attachments"
_HELPER = Path(__file__).resolve().with_name("archive_media_extract.py")
_MAX_FILE = 64 << 20
_MAX_TEXT = 256 << 10
_MAX_EXTRACT_MEMORY = 1536 << 20
_MAX_ATTEMPTS = 4
_MAX_EXTRACT_ATTEMPTS = 3
_DIGEST = re.compile(r"[0-9a-f]{64}\Z")
_CODE = re.compile(r"[A-Za-z][A-Za-z0-9_]{1,63}\Z")
_SCOPE = "resource_type=? AND resource_id=? AND download_cid=? AND download_mid=?"


def init(conn):
    """Create the queue and deduplication indexes, without an implicit commit."""
    conn.execute("""CREATE TABLE IF NOT EXISTS attachments (
        id INTEGER PRIMARY KEY,
        cid TEXT NOT NULL,
        mid TEXT NOT NULL,
        resource_type TEXT NOT NULL CHECK(resource_type IN ('mediaId','fileId')),
        resource_id TEXT NOT NULL,
        download_cid TEXT NOT NULL,
        download_mid TEXT NOT NULL,
        name TEXT,
        content_type TEXT,
        status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending','failed','downloaded')),
        failure_reason TEXT,
        attempts INTEGER NOT NULL DEFAULT 0,
        retry_after INTEGER NOT NULL DEFAULT 0,
        local_path TEXT,
        sha256 TEXT,
        size_bytes INTEGER,
        text_content TEXT,
        extraction_status TEXT CHECK(extraction_status IN ('extracted','failed','unsupported')),
        extraction_error TEXT,
        extraction_attempts INTEGER NOT NULL DEFAULT 0,
        extraction_retry_after INTEGER NOT NULL DEFAULT 0,
        UNIQUE(cid, mid, resource_type, resource_id, download_cid, download_mid)
    )""")
    conn.execute("""CREATE INDEX IF NOT EXISTS attachments_resource
                    ON attachments(resource_type, resource_id, download_cid, download_mid, status)""")
    conn.execute("CREATE INDEX IF NOT EXISTS attachments_sha256 ON attachments(sha256)")
    conn.execute("""CREATE TABLE IF NOT EXISTS attachment_profile (
        id INTEGER PRIMARY KEY CHECK(id=1), selector TEXT NOT NULL
    )""")
    # Occurrences preserve the message projection separately from deduplicated
    # download work. A missing reference is evidence, not a synthetic asset ID.
    conn.execute("""CREATE TABLE IF NOT EXISTS attachment_occurrences (
        cid TEXT NOT NULL, mid TEXT NOT NULL, path TEXT NOT NULL,
        source_cid TEXT NOT NULL, source_mid TEXT NOT NULL,
        attachment_id INTEGER REFERENCES attachments(id), reason TEXT,
        CHECK ((attachment_id IS NULL) != (reason IS NULL))
    )""")
    conn.execute("""CREATE UNIQUE INDEX IF NOT EXISTS attachment_occurrence_key ON
        attachment_occurrences(cid,mid,path,source_cid,source_mid,COALESCE(attachment_id,0),COALESCE(reason,''))""")
    # A sparse quote can point back to downloaded assets on the original message.
    # Keep every candidate; do not pretend a multi-image message identifies one image.
    conn.execute('''CREATE VIEW IF NOT EXISTS quoted_attachment_evidence AS
        SELECT DISTINCT q.cid,q.mid,q.source_cid,q.source_mid,a.id AS attachment_id
        FROM attachment_occurrences q JOIN attachment_occurrences s
          ON s.cid=q.source_cid AND s.mid=q.source_mid AND s.source_cid=s.cid AND s.source_mid=s.mid
        JOIN attachments a ON a.id=s.attachment_id AND a.status='downloaded'
        WHERE q.path='/quotedMessage' AND q.reason='image_placeholder_without_reference'
          AND (s.path GLOB '/resourceRefs/[0-9]*' OR s.path GLOB '/resources/[0-9]*'
            OR s.path GLOB '/_verified_detail/resourceRefs/[0-9]*' OR s.path GLOB '/_verified_detail/resources/[0-9]*')''')


def _id(value):
    if type(value) is int:
        value = str(value)
    if (isinstance(value, str) and 0 < len(value) <= 2048 and value == value.strip()
            and all(c.isprintable() for c in value)):
        return value
    return ""


def _object(value):
    return value if isinstance(value, dict) else {}


def _metadata(value, limit):
    if isinstance(value, str):
        return "".join(c for c in value[:limit] if c.isprintable()).strip() or None
    return None


def _reference(ref, cid, mid):
    if not isinstance(ref, dict):
        return None, 'malformed_reference'
    kind = ref.get('resourceIdType', ref.get('type'))
    rid = _id(ref.get('resourceId'))
    if kind not in ('mediaId', 'fileId') or not rid:
        return None, 'unverified_resource_id'
    download = _object(ref.get('download'))
    args = _object(download.get('arguments'))
    if args.get('type', kind) != kind or _id(args.get('resource-id', rid)) != rid:
        return None, 'conflicting_download_reference'
    if kind == 'fileId':
        cid = mid = ''
    else:
        missing = download.get('missing')
        missing = missing if isinstance(missing, list) else ()
        mid = '' if 'message-id' in missing else _id(args.get('message-id', mid))
        cid = '' if 'open-conversation-id' in missing else _id(args.get('open-conversation-id', cid))
        if not cid or not mid:
            return None, 'missing_media_message_context'
    content_type = ref.get('contentType', ref.get('resourceType'))
    return (kind, rid, cid, mid, _metadata(ref.get('name'), 512), _metadata(content_type, 128)), None


def _media_hint(message):
    # A typed image or a textual placeholder is not a verified image ID.
    if message.get('contentType') in (2, '2'):
        return 'image_without_reference'
    for key in ('messageType', 'msgType'):
        if str(message.get(key, '')).lower() in ('image', 'picture', 'file', 'audio', 'video', 'voice'):
            return 'media_without_reference'
    for key in ('text', 'content'):
        part = message.get(key)
        if isinstance(part, str) and part.lstrip().startswith('{'):
            part = _json(part) or part
        if isinstance(part, dict):
            if part.get('contentType') in (2, '2'):
                return 'image_without_reference'
            part = _object(part.get('textContent')).get('text', part.get('text'))
        if isinstance(part, str) and '[图片' in part:
            return 'image_placeholder_without_reference'
    return None


def _occurrences(message, cid, mid, path='', depth=0):
    if depth > 32 or not isinstance(message, dict):
        yield path, cid, mid, None, 'nesting_limit' if depth > 32 else 'unparsed_message'
        return
    has_reference = False
    for key in ('resourceRefs', 'resources'):
        refs = message.get(key)
        if refs is None:
            continue
        if not isinstance(refs, list):
            yield f'{path}/{key}', cid, mid, None, 'malformed_reference_list'
            continue
        for index, ref in enumerate(refs):
            asset, reason = _reference(ref, cid, mid)
            has_reference |= asset is not None
            yield f'{path}/{key}/{index}', cid, mid, asset, reason
    if not has_reference:
        reason = _media_hint(message)
        if reason:
            yield path, cid, mid, None, reason
    for key in ('quotedMessage', 'forwarded', 'messages'):
        children = message.get(key)
        if children is None:
            continue
        entries = enumerate(children) if isinstance(children, list) else [(None, children)]
        for index, child in entries:
            item = _object(child)
            child_cid = _id(item.get('openConversationId')) or _id(item.get('conversationId')) or cid
            child_mid = _id(item.get('openMessageId')) or _id(item.get('messageId'))
            child_path = f'{path}/{key}' if index is None else f'{path}/{key}/{index}'
            yield from _occurrences(child, child_cid, child_mid, child_path, depth + 1)
    if not depth and isinstance(message.get('_verified_detail'), dict):
        detail = message['_verified_detail']
        detail_mid = _id(detail.get('messageId')) or _id(detail.get('openMessageId'))
        detail_cid = _id(detail.get('conversationId')) or _id(detail.get('openConversationId'))
        if detail_mid == mid and (not detail_cid or detail_cid == cid):
            yield from _occurrences(detail, cid, mid, '/_verified_detail', depth + 1)
        else:
            yield '/_verified_detail', cid, mid, None, 'detail_identity_mismatch'


def enqueue(conn, cid, mid, message):
    """Associate DWS resource references with (cid, mid); do not connect or commit."""
    cid, mid = _id(cid), _id(mid)
    if not cid or not mid:
        raise ValueError("invalid attachment message identity")
    for path, source_cid, source_mid, ref, reason in _occurrences(message, cid, mid):
        attachment_id = None
        if ref is not None:
            attachment_id, = conn.execute("""INSERT INTO attachments
                (cid,mid,resource_type,resource_id,download_cid,download_mid,name,content_type)
                VALUES (?,?,?,?,?,?,?,?)
                ON CONFLICT(cid,mid,resource_type,resource_id,download_cid,download_mid) DO UPDATE SET
                    name=COALESCE(attachments.name,excluded.name),
                    content_type=COALESCE(attachments.content_type,excluded.content_type)
                RETURNING id""", (cid, mid, *ref)).fetchone()
            if ref[0] == 'mediaId':
                source_cid, source_mid = ref[2:4]
            # A detail lookup can supply the reference missing from an index
            # projection. Resolve only this node, never a sibling's image hint.
            node = path.rsplit('/', 2)[0].removeprefix('/_verified_detail')
            conn.execute('''DELETE FROM attachment_occurrences WHERE cid=? AND mid=? AND attachment_id IS NULL
                AND source_cid=? AND source_mid=? AND (path=? OR (path IN (?,?)
                AND reason IN ('media_without_reference','image_without_reference','image_placeholder_without_reference')))''',
                         (cid, mid, source_cid, source_mid, path, node, '/_verified_detail' + node))
        elif reason in ('media_without_reference', 'image_without_reference', 'image_placeholder_without_reference'):
            # Conversely, a later sparse projection cannot undo a known ref.
            node = path.removeprefix('/_verified_detail')
            patterns = tuple(prefix + node + key for prefix in ('', '/_verified_detail')
                             for key in ('/resources/*', '/resourceRefs/*'))
            if conn.execute('''SELECT 1 FROM attachment_occurrences WHERE cid=? AND mid=? AND attachment_id IS NOT NULL
                AND source_cid=? AND source_mid=? AND (path GLOB ? OR path GLOB ? OR path GLOB ? OR path GLOB ?) LIMIT 1''',
                            (cid, mid, source_cid, source_mid, *patterns)).fetchone():
                continue
        conn.execute('''INSERT INTO attachment_occurrences VALUES (?,?,?,?,?,?,?) ON CONFLICT DO NOTHING''',
                     (cid, mid, path, source_cid, source_mid, attachment_id, reason))


class _Failure(Exception):
    def __init__(self, reason, retry=True, retry_delay=0):
        super().__init__(reason)
        self.retry = retry
        self.retry_delay = retry_delay


def _fingerprint(info):
    return info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns


def _hash(path, dir_fd=None):
    if os.name == "nt":
        if dir_fd is not None:
            path = dir_fd / path
        before = os.lstat(path)
        if before.st_file_attributes & stat.FILE_ATTRIBUTE_REPARSE_POINT or not stat.S_ISREG(before.st_mode):
            raise _Failure("unsafe_file", retry=False)
        fd = os.open(path, os.O_RDONLY | os.O_BINARY)
    else:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=dir_fd)
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if os.name == "nt" and not os.path.samestat(before, info):
            raise _Failure("file_changed_during_hash")
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
            raise _Failure("unsafe_file", retry=False)
        if info.st_size > _MAX_FILE:
            raise _Failure("file_too_large", retry=False)
        digest = hashlib.sha256()
        count = 0
        while chunk := stream.read(256 << 10):
            count += len(chunk)
            if count > _MAX_FILE:
                raise _Failure("file_too_large", retry=False)
            digest.update(chunk)
        if count != info.st_size or _fingerprint(info) != _fingerprint(os.fstat(stream.fileno())):
            raise _Failure("file_changed_during_hash")
        return digest.hexdigest(), count


def _verified(fd, digest, size):
    try:
        return _hash(digest, fd) == (digest, size)
    except (OSError, _Failure):
        return False


def _darwin_memory_sampler():
    # Darwin does not enforce a usable RLIMIT_AS on this platform. Sample the
    # physical footprint of the extraction process group, including Swift.
    import ctypes as c

    lib = c.CDLL("/usr/lib/libSystem.B.dylib")
    lib.proc_listpids.argtypes = (c.c_uint32, c.c_uint32, c.c_void_p, c.c_int)
    lib.proc_pid_rusage.argtypes = (c.c_int, c.c_int, c.c_void_p)
    pids = (c.c_int * 32)()
    usage = (c.c_uint64 * 12)()  # Darwin rusage_info_v0; slot 9 is ri_phys_footprint.

    def sample(pgid):
        count = lib.proc_listpids(2, pgid, pids, c.sizeof(pids)) // c.sizeof(c.c_int)
        if count == len(pids):
            return _MAX_EXTRACT_MEMORY + 1
        return sum(usage[9] for i in range(count) if lib.proc_pid_rusage(pids[i], 0, usage) == 0)

    return sample


def _wait(child, timeout, memory_limit):
    if memory_limit is None or sys.platform != "darwin":
        child.wait(timeout=timeout)
        return
    sample = _darwin_memory_sampler()
    deadline = time.monotonic() + timeout
    while child.poll() is None:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise subprocess.TimeoutExpired(child.args, timeout)
        try:
            child.wait(timeout=min(0.2, remaining))
        except subprocess.TimeoutExpired:
            if sample(child.pid) > memory_limit:
                raise _Failure("memory_limit")


def _run(arguments, cwd, timeout, limit, memory_limit=None):
    # File-backed stdio avoids unbounded pipes. POSIX applies a hard file limit;
    # Windows also watches staging files while the kill-on-close job is running.
    with tempfile.TemporaryFile(dir=cwd) as stdout, tempfile.TemporaryFile(dir=cwd) as stderr:
        with subprocess.Popen(arguments, cwd=cwd, stdin=subprocess.DEVNULL,
                              stdout=stdout, stderr=stderr, start_new_session=os.name != "nt",
                              creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0)) as child:
            try:
                if os.name == "nt":
                    deadline = time.monotonic() + timeout
                    while child.poll() is None:
                        if time.monotonic() >= deadline:
                            raise subprocess.TimeoutExpired(arguments, timeout)
                        # Windows has no RLIMIT_FSIZE. Check every staging file,
                        # plus anonymous stdout/stderr; hashes verify final bytes.
                        with os.scandir(cwd) as files:
                            too_large = any(p.stat(follow_symlinks=False).st_size > _MAX_FILE for p in files)
                        if (too_large or os.fstat(stdout.fileno()).st_size > limit
                                or os.fstat(stderr.fileno()).st_size > (64 << 10)):
                            raise _Failure("staging_size_limit", retry=False)
                        try:
                            child.wait(timeout=0.1)
                        except subprocess.TimeoutExpired:
                            pass
                else:
                    _wait(child, timeout, memory_limit)
            except BaseException:
                try:
                    if os.name == "nt":
                        child.kill()  # worker's kill-on-close Job also owns its readers and DWS
                    else:
                        os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                child.wait()
                raise
        stdout.seek(0)
        stderr.seek(0)
        return child.returncode, stdout.read(limit + 1), stderr.read(64 << 10)


def _json(data):
    try:
        return _object(json.loads(data))
    except (ValueError, RecursionError):
        return {}


def _dws_error(rc, reply, stderr):
    detail = _json(stderr)
    fields = (detail, _object(detail.get("error")), reply, _object(reply.get("error")))
    # Expose the actionable category without echoing a message ID, resource URL or filename.
    for field in fields:
        reason = field.get("message")
        if isinstance(reason, str) and "未找到精确匹配的源消息" in reason:
            return _Failure("dws:source_message_not_verified", retry=False)
        if isinstance(reason, str) and "节点不存在或无权访问" in reason:
            return _Failure("dws:file_missing_or_forbidden", retry=False)
    codes = [obj[key] for obj in fields for key in ("reason", "server_error_code", "code")
             if isinstance(obj.get(key), str) and _CODE.fullmatch(obj[key])]
    if os.name != "nt" and rc == -signal.SIGXFSZ:
        return _Failure("download:file_too_large", retry=False)
    retry = any(obj.get("retryable") is True for obj in fields) and not any(
        obj.get("retryable") is False for obj in fields)
    if any(code.upper() == "RESOURCE_NOT_FOUND" for code in codes):
        retry = False
    delay = max((obj["retry_after_seconds"] for obj in fields
                 if type(obj.get("retry_after_seconds")) is int and 0 <= obj["retry_after_seconds"] <= 86400),
                default=0)
    return _Failure("dws:" + ("/".join(dict.fromkeys(codes)) if codes else f"exit_{rc}"), retry, delay)


def _download(dws, profile, scope, stage):
    kind, rid, cid, mid = scope
    argv = [dws, "chat", "+messages-resource-download", "--format", "json",
            f"--type={kind}", f"--resource-id={rid}", "--output=asset.bin"]
    if kind == "mediaId":
        if not cid or not mid:
            raise _Failure("missing_media_message_context", retry=False)
        argv += [f"--message-id={mid}", f"--open-conversation-id={cid}"]
    if profile:
        argv.append(f"--profile={profile}")
    try:
        rc, stdout, stderr = _run(
            [sys.executable, "-I", "-B", str(_HELPER), "limit", str(_MAX_FILE), *argv],
            stage, 90, 64 << 10)
    except subprocess.TimeoutExpired as exc:
        raise _Failure("download:timeout") from exc
    reply = _json(stdout)
    if rc or reply.get("success") is False or reply.get("ok") is False:
        raise _dws_error(rc, reply, stderr)
    receipt = _object(reply.get("result", reply))
    if (receipt.get("localPath") != "asset.bin" or receipt.get("resourceId") != rid
            or receipt.get("resourceType") != kind
            or (kind == "mediaId" and (receipt.get("messageId") != mid or receipt.get("messageVerified") is not True))
            or type(receipt.get("sizeBytes")) is not int):
        raise _Failure("download:invalid_receipt")
    path = stage / "asset.bin"
    digest, size = _hash(path)
    if size != receipt["sizeBytes"]:
        raise _Failure("download:size_mismatch")
    return path, digest, size


def _extract(path, name, stage, previous_attempts):
    attempts = previous_attempts + 1
    retry_at = int(time.time()) + (60 << previous_attempts)
    try:
        # Only a suffix is needed to select a legacy/text reader. Keep the real
        # display name out of subprocess command lines and their diagnostics.
        suffix = Path(name or "").suffix.lower()
        hint = "asset" + suffix if re.fullmatch(r"\.[a-z0-9]{1,8}", suffix) else "asset"
        rc, out, err = _run([sys.executable, "-I", "-B", str(_HELPER), "extract", str(path), hint],
                            stage, 60, _MAX_TEXT, _MAX_EXTRACT_MEMORY)
        if rc == 0 and len(out) <= _MAX_TEXT:
            text = out.decode("utf-8", "replace")
            if not text.strip():
                return None, "failed", "no_readable_text", attempts, retry_at
            return text, "extracted", None, attempts, 0
        if rc == 3:
            return None, "unsupported", None, attempts, 0
        reason = err.decode("ascii", "replace").strip()
        if not re.fullmatch(r"[a-zA-Z0-9_:]{1,80}", reason):
            reason = f"exit_{rc}" if rc else "output_limit"
    except subprocess.TimeoutExpired:
        reason = "timeout"
    except _Failure as exc:
        reason = str(exc)
    except OSError as exc:
        reason = f"io_{exc.errno}"
    return None, "failed", reason, attempts, retry_at


def _pages(conn, last_id, where, start_ms, end_ms):
    cursor = 0
    while True:
        ids = conn.execute(f"""SELECT a.id FROM attachments a JOIN messages m ON (a.cid,a.mid)=(m.cid,m.mid)
            WHERE a.id>? AND a.id<=? AND m.occurred_at>=? AND m.occurred_at<? AND ({where})
            ORDER BY a.id LIMIT 256""", (cursor, last_id, start_ms, end_ms)).fetchall()
        if not ids:
            break
        for (cursor,) in ids:
            yield cursor


def _reset_invalid(conn, store_fd, last_id, transaction):
    """Verify each on-disk blob once, then invalidate every affected association."""
    cursor = ""
    while True:
        blobs = conn.execute("""SELECT sha256, MIN(local_path), MAX(local_path), MIN(size_bytes),
                   MAX(size_bytes), COUNT(local_path)=COUNT(*) AND COUNT(size_bytes)=COUNT(*)
            FROM attachments WHERE typeof(sha256)='text' AND sha256>? AND status='downloaded' AND id<=?
            GROUP BY sha256 ORDER BY sha256 LIMIT 256""", (cursor, last_id)).fetchall()
        if not blobs:
            break
        for cursor, first_path, last_path, smallest, largest, all_set in blobs:
            if not _DIGEST.fullmatch(cursor):
                continue
            try:
                digest, size = _hash(cursor, store_fd)
            except (OSError, _Failure):
                digest, size = None, None
            path = f"{_STORE}/{cursor}"
            if digest != cursor:
                with transaction():
                    _reset(conn, "sha256=?", (cursor,))
            elif not (all_set and first_path == last_path == path and smallest == largest == size):
                with transaction():
                    _reset(conn, "sha256=? AND (local_path IS NOT ? OR size_bytes IS NOT ?)", (cursor, path, size))
    with transaction():
        _reset(conn, "typeof(sha256)!='text' OR length(sha256)!=64 OR sha256 GLOB '*[^0-9a-f]*'", ())


def _reset(conn, where, parameters):
    conn.execute(f"""UPDATE attachments SET status='pending', failure_reason='integrity:disk_verification_failed',
        local_path=NULL, sha256=NULL, size_bytes=NULL, text_content=NULL, extraction_status=NULL,
        extraction_error=NULL, extraction_attempts=0, extraction_retry_after=0, attempts=0, retry_after=0
        WHERE status='downloaded' AND ({where})""", parameters)


_EXTRACTION = "text_content,extraction_status,extraction_error,extraction_attempts,extraction_retry_after"
_SET_EXTRACTION = "text_content=?,extraction_status=?,extraction_error=?,extraction_attempts=?,extraction_retry_after=?"


def _drain(conn, store, store_fd, dws, profile, transaction, start_ms, end_ms):
    last_id = conn.execute("SELECT COALESCE(MAX(id),0) FROM attachments").fetchone()[0]
    _reset_invalid(conn, store_fd, last_id, transaction)
    now = int(time.time())
    for rowid in _pages(conn, last_id, "status!='downloaded'", start_ms, end_ms):
        kind, rid, cid, mid, name, status, attempts, retry_at = conn.execute("""SELECT
            resource_type,resource_id,download_cid,download_mid,name,status,attempts,retry_after
            FROM attachments WHERE id=?""", (rowid,)).fetchone()
        if status == "downloaded":  # Another association resolved this exact download scope.
            continue
        scope = kind, rid, cid, mid
        cached = conn.execute(f"""SELECT local_path,sha256,size_bytes,{_EXTRACTION} FROM attachments
            WHERE {_SCOPE} AND status='downloaded' LIMIT 1""", scope).fetchone()
        if cached is not None:
            # All downloaded rows were verified at the start of the drain, or
            # were published here under the exclusive archive lock.
            result = tuple(cached)
        else:
            if status == "pending":
                prior = conn.execute(f"""SELECT attempts,retry_after,failure_reason FROM attachments
                    WHERE {_SCOPE} AND status='failed' ORDER BY retry_after DESC LIMIT 1""", scope).fetchone()
                if prior is not None:
                    attempts, retry_at, _ = prior
                    if attempts >= _MAX_ATTEMPTS or retry_at > now:
                        with transaction():
                            conn.execute(f"""UPDATE attachments SET status='failed', attempts=?,retry_after=?,failure_reason=?
                                WHERE {_SCOPE} AND status='pending'""", (*prior, *scope))
                        continue
            if retry_at > now or attempts >= _MAX_ATTEMPTS:
                continue
            try:
                # DWS verifies a mediaId by fetching its source message. A
                # forwarded/quoted reference is not authorization for an old source.
                if kind == "mediaId" and cid and mid and not conn.execute(
                    "SELECT 1 FROM messages WHERE cid=? AND mid=? AND occurred_at>=? AND occurred_at<?",
                    (cid, mid, start_ms, end_ms),
                ).fetchone():
                    with transaction():
                        conn.execute("""UPDATE attachments SET status='failed',
                            failure_reason='download:source_outside_window',retry_after=? WHERE id=?""",
                                     (2**62, rowid))
                    continue
                if shutil.disk_usage(store).free < _MAX_FILE + (16 << 20):
                    raise _Failure("download:insufficient_disk_space")
                with tempfile.TemporaryDirectory(prefix=".stage-", dir=store) as tmp:
                    stage = Path(tmp)
                    path, digest, size = _download(dws, profile, scope, stage)
                    extracted = conn.execute(f"SELECT {_EXTRACTION} FROM attachments WHERE sha256=? LIMIT 1",
                                             (digest,)).fetchone()
                    if extracted is None:
                        extracted = _extract(path, name, stage, 0)
                    if not _verified(store_fd, digest, size):
                        # Replace only the digest-named entry, never a symlink's referent.
                        if os.name == "nt":
                            os.replace(path, store / digest)
                        else:
                            os.replace(path, digest, dst_dir_fd=store_fd)
                    result = (f"{_STORE}/{digest}", digest, size, *extracted)
            except (OSError, _Failure) as exc:
                reason = str(exc) if isinstance(exc, _Failure) else f"download:io_{exc.errno}"
                delay = 30 << attempts
                if isinstance(exc, _Failure):
                    delay = max(delay, exc.retry_delay) if exc.retry else 2**62
                with transaction():
                    conn.execute(f"""UPDATE attachments SET status='failed', failure_reason=?, attempts=?, retry_after=?
                        WHERE {_SCOPE} AND status!='downloaded'""",
                        (reason, attempts + 1, int(time.time()) + delay, *scope))
                continue
        with transaction():
            conn.execute(f"""UPDATE attachments SET status='downloaded', local_path=?,sha256=?,size_bytes=?,
                {_SET_EXTRACTION},failure_reason=NULL,retry_after=0 WHERE {_SCOPE} AND status!='downloaded'""",
                (*result, *scope))

    for rowid in _pages(conn, last_id, "extraction_status='failed'", start_ms, end_ms):
        digest, name, attempts, retry_at, state = conn.execute("""SELECT sha256,name,extraction_attempts,
            extraction_retry_after,extraction_status FROM attachments WHERE id=?""", (rowid,)).fetchone()
        if state != "failed" or attempts >= _MAX_EXTRACT_ATTEMPTS or retry_at > now:
            continue
        with tempfile.TemporaryDirectory(prefix=".stage-", dir=store) as tmp:
            result = _extract(store / digest, name, Path(tmp), attempts)
        with transaction():
            conn.execute(f"UPDATE attachments SET {_SET_EXTRACTION} WHERE sha256=?", (*result, digest))

    counts = dict.fromkeys(("pending", "failed", "downloaded"), 0)
    counts.update(conn.execute("SELECT status,COUNT(*) FROM attachments GROUP BY status"))
    counts["extraction_failed"] = conn.execute("SELECT COUNT(*) FROM attachments WHERE extraction_status='failed'").fetchone()[0]
    counts['extraction_unsupported'] = conn.execute("SELECT COUNT(*) FROM attachments WHERE extraction_status='unsupported'").fetchone()[0]
    counts['unresolved_occurrences'] = conn.execute('SELECT COUNT(*) FROM attachment_occurrences WHERE reason IS NOT NULL').fetchone()[0]
    return counts


@contextmanager
def _store_lock(store):
    if os.name == "nt":
        import msvcrt

        if os.lstat(store).st_file_attributes & stat.FILE_ATTRIBUTE_REPARSE_POINT:
            raise PermissionError("attachment directory must not be a reparse point")
        with (store / ".media.lock").open("ab") as lock:
            lock.seek(0)
            msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1)
            yield store
        return

    import fcntl

    fd = os.open(store, os.O_DIRECTORY | os.O_NOFOLLOW | os.O_RDONLY)
    try:
        info = os.fstat(fd)
        if info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise PermissionError("attachment directory must be private and owned by the current user")
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield fd
    finally:
        os.close(fd)


def drain(conn, root, dws, profile, start_ms, end_ms):
    """Verify saved bytes and process queued/due attachments once; return status counts.

    Transient downloads back off from 30 s, up to four total attempts. A terminal
    failure requires an operator to fix the cause and reset its retry fields.
    """
    selector = profile or ""
    executable = shutil.which(os.fspath(dws))
    if executable is None:
        raise FileNotFoundError(errno.ENOENT, "DWS executable not found")
    executable = os.path.abspath(executable)
    store = Path(root).resolve(strict=True) / _STORE
    store.mkdir(mode=0o700, exist_ok=True)
    with _store_lock(store) as fd:
        transaction = nullcontext if conn.in_transaction else lambda: conn
        with transaction():
            conn.execute("INSERT OR IGNORE INTO attachment_profile VALUES (1,?)", (selector,))
            if conn.execute("SELECT selector FROM attachment_profile WHERE id=1").fetchone()[0] != selector:
                raise ValueError("attachment archive was bound to a different DWS profile")
        return _drain(conn, store, fd, executable, selector, transaction, start_ms, end_ms)
