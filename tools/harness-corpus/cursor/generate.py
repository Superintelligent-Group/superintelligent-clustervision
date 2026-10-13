#!/usr/bin/env python3
"""One bounded native CLI scenario. Requires an explicitly authorized IDE access-token DB.

Never logs token/output, writes real stores, or uses the default credential manager.
The installed build must support AGENT_CLI_CREDENTIAL_STORE=memory (2026.10.01).
"""
import argparse
import datetime
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import tempfile
import sys
sys.path.insert(0, str(Path(__file__).resolve().parent))


def main():
    p = argparse.ArgumentParser()
    p.add_argument('--bin', type=Path, required=True)
    p.add_argument('--version', required=True)
    p.add_argument('--out', type=Path, required=True)
    p.add_argument('--ide-auth-db', type=Path, required=True,
                   help='authorized read-only IDE state.vscdb containing cursorAuth/accessToken')
    args = p.parse_args()
    if args.version != '2026.10.01-e373342':
        p.error('memory-only credential behavior is verified only for 2026.10.01-e373342')
    if args.out.exists():
        p.error('refusing to regenerate an existing corpus version')
    executable = args.bin.resolve(strict=True)
    auth = args.ide_auth_db.resolve(strict=True)
    with sqlite3.connect(auth.as_uri() + '?mode=ro&immutable=1', uri=True) as db:
        row = db.execute("SELECT value FROM ItemTable WHERE key='cursorAuth/accessToken'").fetchone()
    if not row or not row[0]:
        p.error('authorized IDE store has no access token')
    token = row[0]
    # Import scrub logic after naming the source home, for owner/account/path redaction.
    os.environ['CORPUS_REAL_HOME'] = str(Path.home())
    import codec
    with tempfile.TemporaryDirectory(prefix='cv-cursor-corpus-') as scratch:
        base = Path(scratch)
        for name in ('home', 'config', 'data', 'cache', 'tmp', 'project'):
            (base / name).mkdir()
        (base / 'project/main.py').write_text('def add(a, b):\n    return a - b\n\nassert add(2, 3) == 5\nprint("5")\n')
        env = {k: v for k, v in os.environ.items()
               if not any(t in k.upper() for t in ('TOKEN', 'API_KEY', 'PASSWORD'))}
        env.update(HOME=str(base / 'home'), TMPDIR=str(base / 'tmp'),
                   XDG_CONFIG_HOME=str(base / 'config'), XDG_DATA_HOME=str(base / 'data'),
                   XDG_CACHE_HOME=str(base / 'cache'), CURSOR_CONFIG_DIR=str(base / 'config/cursor'),
                   CURSOR_DATA_DIR=str(base / 'data/cursor'), AGENT_CLI_CREDENTIAL_STORE='memory',
                   DIRENV_DISABLE='1', CI='1', CURSOR_AGENT_PERSIST_SESSION='1')
        version = subprocess.run([str(executable), '--version'], cwd=base / 'project', env=env,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=15)
        if version.returncode or version.stdout.decode().strip() != args.version:
            raise SystemExit('executable version does not match verified memory-only build; no model invoked')
        env['CURSOR_AUTH_TOKEN'] = token
        task = ('Read main.py. Fix add(a,b) so it returns the sum, then run python3 main.py. '
                'Work only in this scratch directory. Report the result in one sentence and stop.')
        proc = subprocess.Popen([str(executable), '--print', '--force', '--trust', '--output-format', 'json', task],
                                cwd=base / 'project', env=env, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, start_new_session=True)
        try:
            stdout, _ = proc.communicate(timeout=150)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGTERM)
            try:
                proc.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
                proc.communicate()
            raise SystemExit('Cursor scratch task exceeded 150 seconds; no corpus published')
        del env['CURSOR_AUTH_TOKEN']
        token = None
        if proc.returncode:
            raise SystemExit('Cursor scratch task failed; output withheld to protect credentials')
        # Validate native result without persisting model output or account credentials.
        json.loads(stdout)
        stores = list((base / 'config/cursor/chats').rglob('store.db'))
        if len(stores) != 1:
            raise SystemExit('expected exactly one native scratch store')
        args.out.mkdir(parents=True)
        src = stores[0]
        dst = args.out / '.cursor/chats' / src.parent.parent.name / src.parent.name / 'store.db'
        dst.parent.mkdir(parents=True)
        with sqlite3.connect(src.as_uri() + '?mode=ro', uri=True) as source, sqlite3.connect(dst) as dest:
            source.backup(dest)
        codec.scrub_store(dst, base, base / 'home')
        codec.rehash_store(dst)
        with sqlite3.connect(dst) as db:
            assistant_steps = sum(1 for (data,) in db.execute('SELECT data FROM blobs')
                                  if data.startswith(b'{') and json.loads(data).get('role') == 'assistant')
        (args.out / 'MANIFEST.toml').write_text(
            '# Native Cursor Agent CLI scratch capture. Never regenerate a committed version.\n'
            f'harness = "cursor"\nversion = "{args.version}"\ngenerated = "{datetime.date.today()}"\n'
            f'model = "native default"\nmodel_calls = {assistant_steps} # stored assistant completion steps\ncli_invocations = 1\n'
            'scenarios = ["chat", "read", "write", "shell", "tool_results"]\n'
            'skipped = ["resume", "compaction", "subagents", "ACP"]\n'
            'auth = "authorized IDE access token in memory-only child environment; no refresh or keychain writes"\n'
            'scrub = "protocol-aware codec.py; paths/accounts/key scrubbed, SHA-256 IDs and binary references rebuilt"\n')
        subprocess.run(['python3', '-I', str(Path(__file__).resolve().parent.parent / 'scrub.py'),
                        '--work', str(base), '--home', str(base / 'home'), '--dir', str(args.out.resolve())],
                       check=True)
        print('Native Cursor corpus captured and scrubbed; generate its new golden summary with CV_REGEN_CORPUS=1.')


if __name__ == '__main__':
    main()
