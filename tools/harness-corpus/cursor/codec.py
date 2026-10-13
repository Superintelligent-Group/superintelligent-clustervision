"""Scrub native Cursor CLI blobs without breaking protobuf length prefixes.

Content hashes and protobuf references are rebuilt after scrubbing.
Only the copied store is writable. JSON, hex metadata and nested wire messages
are scrubbed before the common corpus scanner sees them.
"""
import hashlib
import importlib.util
import json
import pathlib
import sqlite3

spec = importlib.util.spec_from_file_location('corpus_scrub', pathlib.Path(__file__).parent.parent / 'scrub.py')
scrub = importlib.util.module_from_spec(spec)
spec.loader.exec_module(scrub)


def varint(data, at):
    value = 0
    for shift in range(0, 70, 7):
        if at == len(data):
            raise ValueError('truncated varint')
        b = data[at]
        at += 1
        if shift == 63 and b > 1:
            raise ValueError('overflowing varint')
        value |= (b & 127) << shift
        if b < 128:
            return value, at
    raise ValueError('overflowing varint')


def encode(value):
    out = bytearray()
    while value > 127:
        out.append((value & 127) | 128)
        value >>= 7
    out.append(value)
    return bytes(out)


def wire(data, reps, depth=0, root=False, resolve=None):
    if depth > 64:
        raise ValueError('nested wire depth')
    out, at = bytearray(), 0
    while at < len(data):
        tag, at = varint(data, at)
        field, kind = tag >> 3, tag & 7
        if not 0 < field < (1 << 29):
            raise ValueError('invalid field')
        out += encode(tag)
        if kind == 0:
            value, at = varint(data, at)
            out += encode(value)
            continue
        if kind == 2:
            size, at = varint(data, at)
        elif kind in (1, 5):
            size = 8 if kind == 1 else 4
        else:
            raise ValueError('unsupported wire type')
        value = data[at:at + size]
        if len(value) != size:
            raise ValueError('truncated payload')
        at += size
        if kind == 2:
            # Root field 38 is the service account ID, not a conversation ID.
            if resolve is not None and len(value) == 32 and resolve(value.hex()) is not None:
                value = bytes.fromhex(resolve(value.hex()))
            elif depth == 0 and field == 38 and (root or data.startswith(b'\x0a\x20')):
                value = b'00000000-0000-0000-0000-000000000000'
            else:
                # Try nested wire structure before text: a nested message may be
                # valid UTF-8 while containing its own lengths that must change.
                try:
                    value = wire(value, reps, depth + 1, resolve=resolve)
                except ValueError:
                    try:
                        value.decode('utf-8')
                        value = scrub.fix(value, reps)
                    except UnicodeDecodeError:
                        pass  # opaque hashes/binary signatures
            out += encode(len(value))
        out += value
    return bytes(out)


def json_value(value):
    if isinstance(value, dict):
        return {key: ('corpus-account' if key in ('userId', 'accountId', 'creatorUserId') else json_value(v))
                for key, v in value.items()}
    if isinstance(value, list):
        return [json_value(v) for v in value]
    return value


def length_fields(data):
    at = 0
    while at < len(data):
        tag, at = varint(data, at)
        kind = tag & 7
        if kind == 0:
            _, at = varint(data, at)
            continue
        if kind == 2:
            n, at = varint(data, at)
        elif kind in (1, 5):
            n = 8 if kind == 1 else 4
        else:
            raise ValueError('unsupported wire type')
        value = data[at:at + n]
        if len(value) != n:
            raise ValueError('truncated payload')
        at += n
        if kind == 2:
            yield tag >> 3, value


def scrub_store(path, work, home):
    reps = scrub.replacements(str(work), str(home)) + [('/Users/me', '/home/corpus')]
    with sqlite3.connect(path) as db:
        raw = db.execute("SELECT value FROM meta WHERE key='0'").fetchone()[0]
        meta = json.loads(bytes.fromhex(raw))
        meta.pop('blobEncryptionKey', None)  # local random material; plaintext fixture needs no key
        root_id = meta['latestRootBlobId']
        native_root = db.execute('SELECT data FROM blobs WHERE id=?', (root_id,)).fetchone()[0]
        # Service user IDs also occur in turns/archived snapshots. Replace the
        # selected root's account ID everywhere, preserving all wire lengths.
        for field, value in length_fields(native_root):
            if field == 38 and value:
                reps.append((value.decode('utf-8'), '00000000-0000-0000-0000-000000000000'))
        clean = scrub.fix(json.dumps(json_value(meta)).encode(), reps)
        db.execute("UPDATE meta SET value=? WHERE key='0'", (clean.hex(),))
        for key, data in db.execute('SELECT id,data FROM blobs').fetchall():
            try:
                obj = json.loads(data)
            except (ValueError, UnicodeDecodeError):
                try:
                    clean = wire(data, reps, root=key == root_id)
                    wire(clean, [], root=False)  # exact validation after length changes
                except ValueError:
                    data.decode('utf-8')  # native plain-text file/content blobs
                    clean = scrub.fix(data, reps)
            else:
                clean = scrub.fix(json.dumps(json_value(obj), separators=(',', ':')).encode(), reps)
                json.loads(clean)
            db.execute('UPDATE blobs SET data=? WHERE id=?', (clean, key))
        db.commit()
        db.execute('VACUUM')
        db.execute('PRAGMA journal_mode=DELETE')
    # Check that byte-oriented generic scrubbing would be a no-op for binary cells.
    # It must not rewrite a nested string without rebuilding its lengths.
    with sqlite3.connect(path) as db:
        for (data,) in db.execute('SELECT data FROM blobs'):
            if scrub.fix(data, reps) != data:
                raise ValueError('unhandled binary replacement')


def rehash_store(path):
    """Rebuild the acyclic native content-address graph, then publish fresh tables.

    32-byte wire values matching existing keys are blob references. Unmatched
    hashes remain opaque (e.g. file digests). Fail rather than invent a cycle fix.
    """
    with sqlite3.connect(path) as db:
        blobs = dict(db.execute('SELECT id,data FROM blobs'))
        rewritten, ids, visiting = {}, {}, set()

        def resolve(key):
            if key not in blobs:
                return None
            if key in ids:
                return ids[key]
            if key in visiting:
                raise RuntimeError('cyclic native blob graph')
            visiting.add(key)
            data = blobs[key]
            try:
                json.loads(data)  # messages do not reference binary blob IDs
            except (ValueError, UnicodeDecodeError):
                try:
                    data = wire(data, [], resolve=resolve)
                except ValueError as error:
                    if 'cyclic native' in str(error):
                        raise
                    data.decode('utf-8')  # native plain-text content blob
            new_id = hashlib.sha256(data).hexdigest()
            ids[key], rewritten[new_id] = new_id, data
            visiting.remove(key)
            return new_id

        for key in blobs:
            resolve(key)
        meta = json.loads(bytes.fromhex(db.execute("SELECT value FROM meta WHERE key='0'").fetchone()[0]))
        meta['latestRootBlobId'] = ids[meta['latestRootBlobId']]
        db.execute('DELETE FROM blobs')
        db.executemany('INSERT INTO blobs(id,data) VALUES(?,?)', rewritten.items())
        db.execute("UPDATE meta SET value=? WHERE key='0'", (json.dumps(meta).encode().hex(),))
        db.commit()
        db.execute('VACUUM')
        db.execute('PRAGMA journal_mode=DELETE')
        assert db.execute('PRAGMA freelist_count').fetchone()[0] == 0
        assert all(hashlib.sha256(data).hexdigest() == key for key, data in db.execute('SELECT id,data FROM blobs'))
