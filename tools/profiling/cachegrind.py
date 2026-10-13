#!/usr/bin/env python3
"""Profile one store_probe operation twice; OUT must be new and task-owned."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--input', type=Path, required=True, help='fixture file whose SHA256 is recorded')
    parser.add_argument('--threads', type=int, default=1, help='explicit Rayon worker count')
    parser.add_argument('out', type=Path)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command
    if command and command[0] == '--':
        command = command[1:]
    if not command:
        parser.error('provide a store_probe command after --')
    if args.threads < 1:
        parser.error('--threads must be positive')
    executable = shutil.which(command[0])
    if executable is None:
        parser.error('command executable does not exist or is not executable')
    command[0] = str(Path(executable).resolve())
    fixture = args.input.resolve(strict=True)
    args.out.mkdir(parents=True, exist_ok=False)
    out = args.out.resolve()
    home = out / 'catalog'
    env = dict(os.environ, CLUSTERVISION_HOME=str(home), LC_ALL='C', TZ='UTC',
               RAYON_NUM_THREADS=str(args.threads))
    outputs = []
    counts = []
    for label in ('native', '1', '2'):
        if home.exists():
            shutil.rmtree(home)  # Only this runner's newly created scratch catalog.
        home.mkdir()
        argv = command
        if label != 'native':
            argv = ['valgrind', '--tool=cachegrind', '--cache-sim=no', '--branch-sim=no',
                    '--log-file=' + str(out / f'valgrind-{label}.log'),
                    '--cachegrind-out-file=' + str(out / f'cachegrind-{label}.out'), *command]
        result = subprocess.run(argv, env=env, check=True, capture_output=True)
        (out / f'stdout-{label}.json').write_bytes(result.stdout)
        (out / f'stderr-{label}.txt').write_bytes(result.stderr)
        semantic = json.loads(result.stdout)
        semantic.pop('wall_seconds', None)  # Sole nondeterministic probe instrumentation field.
        outputs.append(semantic)
        if label != 'native':
            profile = (out / f'cachegrind-{label}.out').read_text()
            events = next(line.split()[1:] for line in profile.splitlines() if line.startswith('events:'))
            totals = next(line.split()[1:] for line in profile.splitlines() if line.startswith('summary:'))
            counts.append(int(dict(zip(events, totals))['Ir']))
            with (out / f'annotate-{label}.txt').open('w') as report:
                subprocess.run(['cg_annotate', '--auto=no', '--show=Ir', str(out / f'cachegrind-{label}.out')],
                               stdout=report, check=True, env=env)
    if outputs[0] != outputs[1] or outputs[1] != outputs[2]:
        raise RuntimeError('native/profiled semantic output differs')
    with (out / 'diff.out').open('w') as report:
        subprocess.run(['cg_diff', str(out / 'cachegrind-1.out'), str(out / 'cachegrind-2.out')],
                       stdout=report, check=True, env=env)
    with (out / 'annotate-diff.txt').open('w') as report:
        subprocess.run(['cg_annotate', '--auto=no', '--show=Ir', '--threshold=0', str(out / 'diff.out')],
                       stdout=report, check=True, env=env)
    binary = Path(command[0]).resolve()
    summary = {'command': command, 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
               'input_sha256': hashlib.sha256(fixture.read_bytes()).hexdigest(),
               'rayon_threads': args.threads,
               'runtime_rustflags': os.environ.get('RUSTFLAGS', ''),
               'valgrind': subprocess.check_output(['valgrind', '--version'], text=True).strip(),
               'architecture': os.uname().machine, 'Ir': counts, 'Ir_delta': counts[1] - counts[0],
               'semantic_output': outputs[0], 'parity': True}
    (out / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary, indent=2))


if __name__ == '__main__':
    main()
