"""Inspect recently edited Drive files; never treat a recent edit as task acceptance."""
import base64
import hashlib
from datetime import datetime
from pathlib import Path
from tempfile import TemporaryDirectory

from archive_media import _extract

WINDOW = 7 * 86_400_000
READABLE = {'doc', 'docx', 'xls', 'xlsx', 'ppt', 'pptx', 'pdf', 'txt', 'md', 'png', 'jpg', 'jpeg'}


def inspect_recent(conn, root, call, until):
    # Bind original bytes and the extraction to the verified node version.
    conn.execute('''CREATE TABLE IF NOT EXISTS recent_document_content (
        node TEXT PRIMARY KEY, modified_ms INTEGER NOT NULL, sha256 TEXT NOT NULL,
        original BLOB NOT NULL, extracted_text TEXT, extraction_status TEXT NOT NULL)''')
    conn.execute('''CREATE TABLE IF NOT EXISTS recent_document_evidence (
        node TEXT PRIMARY KEY, name TEXT NOT NULL, modified_ms INTEGER NOT NULL,
        extension TEXT NOT NULL, observation TEXT NOT NULL, units INTEGER NOT NULL)''')
    result = call('drive', '+recent', '--operate-type', '1', '--limit', '50', '--page-all', '--max-pages', '20', '--max-items', '500')
    page = result['meta']['pagination']
    if page.get('endpoint_exhausted') is not True:
        raise ValueError('incomplete recent document index')
    records = []
    for item in result['data']['items']:
        at = datetime.fromisoformat(item['accessTime'].replace('Z', '+00:00'))
        if at.tzinfo is None or not until-WINDOW <= at.timestamp()*1000 <= until:
            continue
        node = item['nodeId']
        info = call('drive', '+inspect', '--node', node)['data']['data']['file']
        stamp = int(info['modifyTime'])
        if info['fileId'] != node or not until-WINDOW <= stamp <= until:
            continue
        ext = str(info.get('extension', '')).lower()
        units, obs = 0, '仅元信息'
        try:
            if ext == 'able':
                tables = call('aitable', '+list-tables', '--base', node)['tables']
                if not isinstance(tables, list):
                    raise ValueError('invalid table directory')
                obs, units = '可读数据表目录（未读行）', len(tables)
            elif ext in READABLE and 0 < int(info.get('fileSize') or 0) <= 32*1024*1024:
                with TemporaryDirectory(dir=root) as tmp:
                    filename = 'asset.' + ext
                    receipt = call('drive', '+download', '--node', node, '--output', filename, cwd=tmp)
                    file = Path(tmp) / filename
                    if receipt.get('ok') is not True or not file.is_file() or file.stat().st_size != int(info['fileSize']):
                        raise ValueError('download receipt mismatch')
                    original = file.read_bytes()  # size checked against the 32 MiB cap above
                    if info.get('md5'):
                        digest = hashlib.md5(original)  # Drive version check, not a signature.
                        if (info['md5'].lower() != digest.hexdigest() and
                                info['md5'] != base64.b64encode(digest.digest()).decode('ascii')):
                            raise ValueError('different document version')
                    content, status, _, _, _ = _extract(file, filename, tmp, 0)
                    with conn:
                        conn.execute('''INSERT OR REPLACE INTO recent_document_content
                            VALUES (?,?,?,?,?,?)''', (node, stamp, hashlib.sha256(original).hexdigest(),
                                                       original, content, status))
                    if status == 'extracted' and content:
                        obs, units = '可读文字（字符）', len(content)
                    else:
                        obs = '已下载，没有可读文字'
        except (OSError, ValueError, RuntimeError, KeyError):
            obs = '内容读取未验证'
        records.append((node, str(info.get('name') or ''), stamp, ext, obs, units))
    with conn:
        conn.execute('DELETE FROM recent_document_evidence')
        conn.executemany('INSERT OR REPLACE INTO recent_document_evidence VALUES (?,?,?,?,?,?)', records)
        conn.execute('''DELETE FROM recent_document_content WHERE NOT EXISTS
            (SELECT 1 FROM recent_document_evidence e WHERE e.node=recent_document_content.node
             AND e.modified_ms=recent_document_content.modified_ms)''')
