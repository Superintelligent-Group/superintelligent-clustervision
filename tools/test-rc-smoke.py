#!/usr/bin/env python3
"""Offline, non-compiling release identity, archive and isolated-smoke controls."""
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('rc_smoke', Path(__file__).with_name('smoke-rc.py'))
RC = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RC)
SHA = 'a'*40
TAG = 'v0.18.0-rc.1'
TARGET = 'aarch64-apple-darwin'
REPO = 'emberian/cv'


class Smoke(unittest.TestCase):
    def test_run_identity_rejects_wrong_tag_sha_repo_and_unsuccessful_runs(self):
        run = dict(id=42, repository={'full_name': REPO}, head_repository={'full_name': REPO},
                   path='.github/workflows/release.yml', event='push', status='completed',
                   conclusion='success', head_branch=TAG, head_sha=SHA)
        RC.validate_run(run, REPO, 42, TAG, SHA)
        for field, value in [('head_branch', 'v0.18.0-rc.2'), ('head_sha', 'b'*40),
                             ('conclusion', 'failure'), ('event', 'pull_request'),
                             ('path', '.github/workflows/ci.yml'), ('id', 43),
                             ('head_repository', {'full_name': 'other/fork'})]:
            with self.subTest(field=field), self.assertRaises(RuntimeError):
                RC.validate_run(dict(run, **{field: value}), REPO, 42, TAG, SHA)
        for tag in ('v0.18.0', '../rc', 'v0.18.0-rc.1/other'):
            with self.assertRaises(RuntimeError):
                RC.validate_run(run, REPO, 42, tag, SHA)

    def test_moved_tag_is_rejected_before_downloading_or_executing(self):
        argv = ['smoke-rc.py', '--repo', REPO, '--run-id', '42', '--tag', TAG,
                '--sha', SHA, '--target', TARGET]
        with patch.object(sys, 'argv', argv), patch.object(RC, 'api'), \
                patch.object(RC, 'validate_run'), patch.object(RC, 'tag_sha', return_value='b'*40), \
                patch.object(RC.subprocess, 'run') as execute:
            with self.assertRaisesRegex(RuntimeError, 'wrong source SHA'):
                RC.main()
            execute.assert_not_called()

    def test_annotated_tag_resolves_to_commit(self):
        with patch.object(RC, 'api', side_effect=[{'object': {'type': 'tag', 'sha': 'b'*40}},
                                                 {'object': {'type': 'commit', 'sha': SHA}}]) as api:
            self.assertEqual(RC.tag_sha(REPO, TAG), SHA)
            self.assertEqual(api.call_args_list[1].args[1], 'git/tags/'+'b'*40)

    def archive(self, root, name=None, kind=None):
        path = root/'asset.tar.xz'
        with tarfile.open(path, 'w:xz') as package:
            member = tarfile.TarInfo(name or f'clustervision-{TARGET}/cv')
            if kind:
                member.type = kind
                member.linkname = '/outside'
            else:
                member.size = len(b'binary')
            package.addfile(member, None if kind else io.BytesIO(b'binary'))
        asset = dict(id=7, name=f'clustervision-{TARGET}.tar.xz', state='uploaded',
                     size=path.stat().st_size, digest='sha256:'+hashlib.sha256(path.read_bytes()).hexdigest())
        return path, asset

    def test_missing_ambiguous_or_digestless_assets_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            _, asset = self.archive(Path(temporary))
            release = dict(tag_name=TAG, prerelease=True, draft=False, assets=[asset])
            self.assertEqual(RC.select_asset(release, TAG, TARGET), asset)
            for assets in ([], [asset, asset], [dict(asset, digest=None)], [dict(asset, digest='sha512:bad')]):
                with self.assertRaises(RuntimeError):
                    RC.select_asset(dict(release, assets=assets), TAG, TARGET)
            with self.assertRaises(RuntimeError):
                RC.select_asset(dict(release, tag_name='wrong'), TAG, TARGET)

    def test_digest_mismatch_fails_before_publication(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive, asset = self.archive(root)
            with self.assertRaisesRegex(RuntimeError, 'digest mismatch'):
                RC.extract_binary(archive, dict(asset, digest='sha256:'+'0'*64), TARGET, root)
            self.assertFalse((root/'cv').exists())
            binary = RC.extract_binary(archive, asset, TARGET, root)
            self.assertEqual(binary.read_bytes(), b'binary')
            self.assertEqual(binary.stat().st_mode & 0o777, 0o700)

    def test_unsafe_archive_paths_and_links_are_never_extracted(self):
        for name, kind in [('/outside', None), ('../outside', None),
                           (f'clustervision-{TARGET}/../cv', None),
                           (f'clustervision-{TARGET}/cv', tarfile.SYMTYPE),
                           (f'clustervision-{TARGET}/cv', tarfile.LNKTYPE),
                           (f'clustervision-{TARGET}/cv', tarfile.FIFOTYPE)]:
            with self.subTest(name=name, kind=kind), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                archive, asset = self.archive(root, name, kind)
                with self.assertRaises(RuntimeError):
                    RC.extract_binary(archive, asset, TARGET, root)
                self.assertFalse((root/'cv').exists())

    def test_smoke_uses_extracted_binary_and_isolates_every_command(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = root/'fixture'
            (fixture/'.claude/projects').mkdir(parents=True)
            transcript = fixture/'.claude/projects/session.jsonl'
            transcript.write_text('fixture')
            case = root/'case'
            case.mkdir()
            binary = root/'extracted/cv'
            calls = []
            def run(argv, **kwargs):
                calls.append((argv, kwargs))
                if argv[1:] == ['--version']:
                    value = f'cv {TAG[1:]} ({SHA[:12]})'.encode()
                elif argv[1] == 'ls':
                    value = json.dumps([{'harness': 'claude', 'id': 'session'}]).encode()
                else:
                    value = b'{"messages":[{"role":"user","content":[]}]}'
                return subprocess.CompletedProcess(argv, 0, value)
            with patch.object(RC.subprocess, 'run', side_effect=run):
                result = RC.smoke(binary, TAG, SHA, case, fixture)
            self.assertEqual(result['rows'], 1)
            self.assertEqual(len(calls), 3)
            for argv, kwargs in calls:
                self.assertEqual(argv[0], str(binary))
                self.assertEqual(kwargs['env']['HOME'], str(case/'home'))
                self.assertNotIn('GH_TOKEN', kwargs['env'])
                self.assertEqual(kwargs['cwd'], case)
            wrong = root/'wrong-source'
            wrong.mkdir()
            with patch.object(RC.subprocess, 'run', side_effect=run):
                with self.assertRaisesRegex(RuntimeError, 'wrong binary source SHA'):
                    RC.smoke(binary, TAG, 'b'*40, wrong, fixture)
            self.assertEqual(transcript.read_text(), 'fixture')

    def test_workflow_uses_native_architectures_and_readonly_release_completion(self):
        text = (Path(__file__).parents[1]/'.github/workflows/macos-rc.yml').read_text()
        for required in ('workflow_run:', 'workflows: [Release]', 'types: [completed]',
                         'runner: macos-15\n', 'runner: macos-15-intel\n',
                         'target: aarch64-apple-darwin', 'target: x86_64-apple-darwin',
                         'contents: read', 'actions: read', 'persist-credentials: false'):
            self.assertIn(required, text)
        self.assertNotIn('cargo ', text)


if __name__ == '__main__':
    unittest.main()
