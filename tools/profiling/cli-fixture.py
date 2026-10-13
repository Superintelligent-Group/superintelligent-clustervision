#!/usr/bin/env python3
"""Build one new mixed synthetic CLI HOME using the existing perf-fixture generator."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('out', type=Path)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=False)
    out = args.out.resolve()
    home = out / 'home'
    home.mkdir()
    generator = Path(__file__).resolve().parents[1] / 'perf-fixture.py'
    sessions = []
    for harness in ('claude', 'codex'):
        for index in range(4):
            generated = out / f'generated-{harness}-{index}'
            manifest = json.loads(subprocess.check_output([
                sys.executable, str(generator), harness, str(generated),
                '--payload-bytes', '65536', '--records', '16',
            ], text=True))
            sid = f'{1 if harness == "claude" else 2:08d}-0000-4000-8000-{index + 1:012d}'
            source = Path(manifest['path'])
            relative = source.relative_to(generated)
            target = home / f'.{harness}' / str(relative).replace(manifest['id'], sid)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(source.read_bytes().replace(manifest['id'].encode(), sid.encode()))
            sessions.append({'harness': harness, 'id': sid, 'path': str(target),
                             'physical_bytes': target.stat().st_size})
            shutil.rmtree(generated)  # Only this script's just-created generator output.
    # Avoid WATCH_FUDGE-triggered rediscovery in the deliberately warm catalog control.
    for path in sorted(home.rglob('*'), reverse=True):
        os.utime(path, (1700000000, 1700000000))
    os.utime(home, (1700000000, 1700000000))
    manifest = {'synthetic': True, 'home': str(home), 'sessions': sessions,
                'generator': 'tools/perf-fixture.py history; 65536 decoded bytes,16 records/session'}
    (out / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
    print(json.dumps(manifest, indent=2))


if __name__ == '__main__':
    main()
