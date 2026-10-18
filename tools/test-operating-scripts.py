#!/usr/bin/env python3
"""No builds/network: exercise operating scripts against disposable command mocks."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

TOOLS = Path(__file__).resolve().parent
SHA = 'a' * 40
BASE = 'b' * 40


class Scripts(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        (self.root / 'dev/cv').mkdir(parents=True)
        self.env = dict(os.environ, HOME=str(self.root), PATH=f'{self.bin}:' + os.environ['PATH'],
                        MOCK_ROOT=str(self.root), SHA=SHA, BASE=BASE)
        self.mock('git', '''
import os, pathlib, sys
p=pathlib.Path(os.environ['MOCK_ROOT']); a=sys.argv[1:]
with (p/'git-calls').open('a') as f: f.write(repr(a)+'\\n')
if a[:1]==['ls-remote']:
 branch=a[-1]; sha=os.environ['BASE'] if branch.endswith('/main') else os.environ['SHA']
 if (p/'dispatched').exists() and os.environ.get('RACE') == branch: sha='c'*40
 print(sha+'\\t'+branch)
elif a[:1]==['merge-base']: sys.exit(1 if os.environ.get('NON_ANCESTOR') else 0)
elif a[:1]==['push']: sys.exit(1 if os.environ.get('LEASE_RACE') else 0)
elif 'rev-parse' in a:
 print(str(p/'cv-remote/wt-test') if '--show-toplevel' in a else os.environ['SHA'])
elif 'status' in a: print(' M preserved' if os.environ.get('DIRTY') else '',end='')
elif 'symbolic-ref' in a: sys.exit(1)
elif a[:2]==['worktree','add']: pathlib.Path(a[-2]).mkdir(parents=True)
''')
        self.mock('gh', '''
import json, os, pathlib, sys
p=pathlib.Path(os.environ['MOCK_ROOT']); a=sys.argv[1:]
if a[:2]==['workflow','run']: (p/'dispatched').touch()
elif a[:2]==['run','view']: print(os.environ['SHA']+' completed '+os.environ.get('FINAL_STATUS','success')+' workflow_dispatch')
else:
 rows=[dict(databaseId=1,headSha=os.environ['SHA'],status='completed',conclusion='success',url='mock')]
 if (p/'dispatched').exists() and not os.environ.get('STALE'):
  rows.append(dict(databaseId=2,headSha=('d'*40 if os.environ.get('WRONG_SHA') else os.environ['SHA']),status='completed',conclusion=os.environ.get('CONCLUSION','success'),url='mock'))
  if os.environ.get('AMBIGUOUS'): rows.append(dict(rows[-1], databaseId=3))
 print(json.dumps(rows))
''')
        self.mock('flock', 'import os,sys; sys.exit(1 if os.environ.get("BUSY") else 0)')
        self.mock('sleep', 'import sys; sys.exit(92)')
        self.mock('cargo', 'import os,sys; sys.exit(7 if os.environ.get("CARGO_FAIL") else 0)')
        # Never run repository Python checks in these tests.
        self.mock('python3', '''
import os,sys
if len(sys.argv)>1 and sys.argv[1]=='-I': sys.exit(0)
os.execv(sys.executable,[sys.executable]+sys.argv[1:])
''')
        self.mock('mdbook', 'pass')
        self.mock('bash', '''
import os,sys
if 'check-manual-names.sh' in ' '.join(sys.argv): sys.exit(0)
os.execv('/bin/bash',['/bin/bash']+sys.argv[1:])
''')

    def mock(self, name, body):
        path = self.bin / name
        path.write_text('#!' + os.sys.executable + '\n' + body)
        path.chmod(0o755)

    def run_script(self, name, *args, **env):
        return subprocess.run(['/bin/bash', str(TOOLS/name), *args], env=dict(self.env, **env),
                              text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)

    def assert_no_push(self):
        self.assertNotIn("['push'", (self.root/'git-calls').read_text())

    def test_success_lands_exact_sha_with_main_lease(self):
        result = self.run_script('land-ci.sh', 'candidate')
        self.assertEqual(result.returncode, 0, result.stdout)
        calls = (self.root/'git-calls').read_text()
        self.assertIn('--force-with-lease=refs/heads/main:' + BASE, calls)
        self.assertIn(SHA + ':refs/heads/main', calls)

    def test_landing_refuses_failed_stale_ambiguous_and_racing_results(self):
        for env in ({'CONCLUSION': 'failure'}, {'STALE': '1'}, {'WRONG_SHA': '1'},
                    {'AMBIGUOUS': '1'}, {'RACE': 'refs/heads/main'},
                    {'RACE': 'refs/heads/candidate'}):
            with self.subTest(env=env):
                (self.root/'dispatched').unlink(missing_ok=True)
                result = self.run_script('land-ci.sh', 'candidate', **env)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assert_no_push()

    def test_non_ancestor_and_direct_status_mismatch_never_push(self):
        for env in ({'NON_ANCESTOR': '1'}, {'FINAL_STATUS': 'failure'}):
            with self.subTest(env=env):
                (self.root/'dispatched').unlink(missing_ok=True)
                result = self.run_script('land-ci.sh', 'candidate', **env)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assert_no_push()

    def test_lease_race_propagates_push_rejection(self):
        result = self.run_script('land-ci.sh', 'candidate', LEASE_RACE='1')
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertNotIn('Landed', result.stdout)
        self.assertIn('--force-with-lease=refs/heads/main:' + BASE,
                      (self.root/'git-calls').read_text())

    def test_supplemental_workflow_cannot_authorize_landing(self):
        result = self.run_script('land-ci.sh', 'candidate', 'macos.yml')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root/'dispatched').exists())

    def test_worker_propagates_failures_and_keeps_warm_target(self):
        target = self.root/'cv-remote/target-test'
        target.mkdir(parents=True)
        sentinel = target/'preserved'
        sentinel.write_text('warm')
        result = self.run_script('remote-gate.sh', '--worker', SHA, 'test', CARGO_FAIL='1')
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn('NOT full CI', result.stdout)
        self.assertEqual(sentinel.read_text(), 'warm')

    def test_worker_preserves_existing_unowned_and_dirty_work(self):
        work = self.root/'cv-remote/wt-test'
        work.mkdir(parents=True)
        sentinel = work/'preserved'
        sentinel.write_text('work')
        for owned in (False, True):
            if owned:
                (work.parent/'owned-test').write_text(str(work)+'\n')
            result = self.run_script('remote-gate.sh', '--worker', SHA, 'test', DIRTY='1')
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertEqual(sentinel.read_text(), 'work')
        self.assertNotIn("'checkout'", (self.root/'git-calls').read_text())

    def test_busy_lane_does_not_touch_worktree(self):
        result = self.run_script('remote-gate.sh', '--worker', SHA, 'test', BUSY='1')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root/'git-calls').exists())

    def test_invalid_arguments_never_reach_network(self):
        result = self.run_script('remote-gate.sh', 'main', 'bad;touch-no')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root/'git-calls').exists())


class CompatibilityCache(unittest.TestCase):
    def test_reuse_is_owned_and_changed_probe_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            repo = root / 'candidate'
            repo.mkdir()
            def git(*args):
                return subprocess.check_output(['git', '-C', str(repo), *args], text=True).strip()
            git('init', '-q')
            git('config', 'user.name', 'Fixture')
            git('config', 'user.email', 'fixture@example.invalid')
            git('config', 'commit.gpgsign', 'false')
            (repo/'Cargo.toml').write_text('version = "0.18.0"\n')
            examples = repo/'crates/cv-core/examples'
            examples.mkdir(parents=True)
            (examples/'existing.rs').write_text('release example')
            git('add', 'crates')
            git('add', 'Cargo.toml')
            git('commit', '-qm', 'release')
            git('tag', 'v0.17.0')
            probe = repo/'crates/cv-core/examples/compatibility_probe.rs'
            probe.parent.mkdir(parents=True, exist_ok=True)
            probe.write_text('fixture probe')
            git('add', str(probe))
            git('commit', '-qm', 'candidate probe')
            commands = root/'bin'
            commands.mkdir()
            for name, body in {
                'cargo': 'import os; assert os.environ.get("RUSTC_WRAPPER", "") == ""',
                'python3': 'import os,sys\nif sys.argv[1:2] != ["-I"]: os.execv(sys.executable,[sys.executable]+sys.argv[1:])',
            }.items():
                path = commands/name
                path.write_text('#!' + os.sys.executable + '\n' + body)
                path.chmod(0o755)
            env = dict(os.environ, PATH=str(commands)+':'+os.environ['PATH'], RUSTC_WRAPPER='',
                       CV_COMPAT_CACHE_ROOT=str(root/'cache'), CARGO_TARGET_DIR=str(root/'target'))
            def run():
                return subprocess.run(['/bin/bash', str(TOOLS/'compatibility-gate.sh')], cwd=repo,
                                      env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            for _ in range(2):
                result = run()
                self.assertEqual(result.returncode, 0, result.stdout)
            caches = list((root/'cache').iterdir())
            self.assertEqual(len(caches), 1)
            baseline = caches[0]/'baseline'
            self.assertTrue((caches[0]/'owner').is_file())
            (baseline/'crates/cv-core/examples/compatibility_probe.rs').write_text('other session changed this')
            result = run()
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn('baseline probe changed', result.stdout)
            self.assertFalse((caches[0]/'lock').exists())


if __name__ == '__main__':
    unittest.main()
