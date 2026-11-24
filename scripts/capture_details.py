"""Enrich degraded list-all projections before archiving, never by guessing an ID."""
import json


def _needs_detail(message):
    if message.get('sendType') == 'unknown':
        return True
    for key in ('text', 'content'):
        value = message.get(key)
        if isinstance(value, str) and value.lstrip().startswith('{'):
            try:
                value = json.loads(value)
            except ValueError:
                continue
        if isinstance(value, dict) and ('contentType' in value or 'textContent' in value):
            return True
    return (not (message.get('resourceRefs') or message.get('resources')) and
            any('[图片' in str(message.get(k) or '') for k in ('text', 'content')))


def hydrate_page(conn, dws, profile, window, groups, dws_json):
    from archive_sync import occurred_ms, reject_partial, SyncError
    conn.execute('''CREATE TABLE IF NOT EXISTS message_detail_receipts(
        cid TEXT NOT NULL, mid TEXT NOT NULL, status TEXT NOT NULL, PRIMARY KEY(cid,mid))''')
    pending = {}
    outcomes = []
    for group in groups:
        for message in group['messages']:
            if not _needs_detail(message):
                continue
            stamp = occurred_ms(message['createTime'])
            if not window['start_ms'] <= stamp < window['end_ms']:
                continue
            key = (message.get('conversationId') or group['openConversationId'], message['messageId'])
            oid = message.get('openMessageId')
            if not isinstance(oid, str) or not oid or ',' in oid:
                outcomes.append((*key, 'missing_open_id'))
                continue
            pending.setdefault(oid, []).append((key,stamp,message))
    ids = list(pending)
    for offset in range(0, len(ids), 50):
        batch = ids[offset:offset+50]
        result = dws_json(dws, 'chat', '+messages-mget', '--msg-ids', ','.join(batch),
                          '--no-reactions', '--no-threads', profile=profile)
        data = result.get('data', result)
        reject_partial(data)
        if data.get('messagesComplete') is False:
            raise SyncError('incomplete DWS message detail page')
        if (not isinstance(data.get('messages'), list) or data.get('hasMore') is not False
                or data.get('requestedCount') != len(batch)):
            raise SyncError('unverified DWS message detail envelope')
        details = {}
        for detail in data['messages']:
            key = (detail.get('conversationId'), detail.get('messageId'))
            if key in details:
                raise SyncError('duplicate DWS message detail identity')
            details[key] = detail
        missing = set(data.get('notFoundMessageIds') or ())
        for oid in batch:
            for key,stamp,message in pending[oid]:
                detail = details.get(key)
                detail_stamp = occurred_ms(detail['createTime']) if detail is not None else None
                sender = message.get('senderId')
                other = detail.get('senderId') if detail is not None else None
                if (detail_stamp is not None and window['start_ms'] <= detail_stamp < window['end_ms']
                        and (not sender or not other or sender == other)
                        and (detail_stamp == stamp or (sender and sender == other))):
                    message['_verified_detail'] = detail
                    status = 'verified' if detail_stamp == stamp else 'verified_with_time_difference'
                elif oid in missing and detail is None:
                    status = 'not_found'
                else:
                    status = 'unverified_identity_or_time'
                outcomes.append((*key,status))
    conn.executemany('''INSERT INTO message_detail_receipts VALUES (?,?,?)
        ON CONFLICT(cid,mid) DO UPDATE SET status=excluded.status''', outcomes)
