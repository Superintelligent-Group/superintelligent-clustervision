"""Offline codec regression; synthetic wire examples, never a native corpus."""
import hashlib
import json
import pathlib
import sqlite3
import tempfile
import unittest
import sys
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import codec


class Codec(unittest.TestCase):
    def test_nested_utf8_lengths_are_rebuilt(self):
        path = b'/scratch/project/a.py'
        leaf = b'\x0a' + codec.encode(len(path)) + path
        outer = b'\x12' + codec.encode(len(leaf)) + leaf
        new = codec.wire(outer, [('/scratch', '/home/corpus')])
        expected_path = b'/home/corpus/project/a.py'
        expected_leaf = b'\x0a' + codec.encode(len(expected_path)) + expected_path
        self.assertEqual(new, b'\x12' + codec.encode(len(expected_leaf)) + expected_leaf)
        self.assertEqual(codec.wire(new, []), new)

    def test_metadata_hex_and_json_and_plain_blobs(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = pathlib.Path(tmp) / 'store.db'
            with sqlite3.connect(p) as db:
                db.executescript('CREATE TABLE blobs(id TEXT PRIMARY KEY,data BLOB); CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT);')
                meta = {'latestRootBlobId':'root', 'blobEncryptionKey':'private-local-material', 'name':'/scratch/home/task'}
                db.execute("INSERT INTO meta VALUES('0',?)", (json.dumps(meta).encode().hex(),))
                db.execute("INSERT INTO blobs VALUES('root',?)", (b'\x08\x01',))
                db.execute("INSERT INTO blobs VALUES('json',?)", (json.dumps({'userId':'private','text':'/scratch/home/a'}).encode(),))
                db.execute("INSERT INTO blobs VALUES('plain',?)", (b'/scratch/home/code',))
            codec.scrub_store(p, '/scratch', '/scratch/home')
            with sqlite3.connect(p) as db:
                meta = json.loads(bytes.fromhex(db.execute('select value from meta').fetchone()[0]))
                self.assertNotIn('blobEncryptionKey', meta)
                self.assertEqual(meta['name'], '/home/corpus/task')
                blobs = dict(db.execute('select id,data from blobs'))
                self.assertEqual(json.loads(blobs['json'])['userId'], 'corpus-account')
                self.assertEqual(blobs['plain'], b'/home/corpus/code')

    def test_rehash_rewrites_root_refs_and_cleans_deleted_cells(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = pathlib.Path(tmp) / 'store.db'
            msg = b'{"role":"user","content":"scrubbed"}'
            old = '11' * 32
            root = b'\x0a\x20' + bytes.fromhex(old)
            root_id = '22' * 32
            with sqlite3.connect(p) as db:
                db.executescript('CREATE TABLE blobs(id TEXT PRIMARY KEY,data BLOB); CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT);')
                db.executemany('INSERT INTO blobs VALUES(?,?)', [(old,msg), (root_id,root)])
                db.execute("INSERT INTO meta VALUES('0',?)", (json.dumps({'latestRootBlobId':root_id}).encode().hex(),))
            codec.rehash_store(p)
            with sqlite3.connect(p) as db:
                blobs = dict(db.execute('SELECT id,data FROM blobs'))
                new_msg = hashlib.sha256(msg).hexdigest()
                self.assertIn(new_msg, blobs)
                self.assertIn(b'\x0a\x20' + bytes.fromhex(new_msg), blobs.values())
                self.assertNotIn(old.encode(), p.read_bytes())
                self.assertEqual(db.execute('PRAGMA freelist_count').fetchone()[0], 0)
                self.assertTrue(all(hashlib.sha256(v).hexdigest() == k for k,v in blobs.items()))

    def test_malformed_wire_is_rejected(self):
        for blob in (b'\x00', b'\x0a\x20x', b'\x80' * 11):
            with self.assertRaises(ValueError):
                codec.wire(blob, [])


if __name__ == '__main__':
    unittest.main()
