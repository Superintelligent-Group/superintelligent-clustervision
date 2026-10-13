#!/usr/bin/env python3
"""Compare two matched Cachegrind summaries; reject changed inputs, state or output."""
import argparse
import json
from pathlib import Path
import statistics


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('before', type=Path)
    parser.add_argument('after', type=Path)
    args = parser.parse_args()
    before = json.loads(args.before.read_text())
    after = json.loads(args.after.read_text())
    fields = ['command', 'input_sha256', 'input_metadata_sha256', 'seed_catalog_sha256',
              'seed_metadata_sha256', 'catalog_home', 'harness_home', 'rayon_threads',
              'catalog_max_stale_secs', 'runtime_rustflags', 'valgrind', 'architecture',
              'output_mode', 'semantic_output', 'stderr_output']
    for field in fields:
        if field not in before or field not in after or before[field] != after[field]:
            raise ValueError(f'unmatched control: {field}')
    build_fields = ['rustc', 'llvm', 'profile', 'core_features', 'rustflags',
                    'image_manifest', 'cpus', 'memory_bytes']
    build_verified = False
    if 'build' in before or 'build' in after:
        if 'build' not in before or 'build' not in after:
            raise ValueError('build metadata present for only one control')
        for field in build_fields:
            if field not in before['build'] or before['build'][field] != after['build'].get(field):
                raise ValueError(f'unmatched build control: {field}')
        build_verified = True
    if not before['parity'] or not after['parity']:
        raise ValueError('native/profiled output parity was not established')
    old = statistics.mean(before['Ir'])
    new = statistics.mean(after['Ir'])
    print(json.dumps({'before_binary_sha256': before['binary_sha256'],
                      'after_binary_sha256': after['binary_sha256'],
                      'before_Ir': before['Ir'], 'after_Ir': after['Ir'],
                      'mean_instruction_reduction_percent': 100 * (1 - new / old),
                      'matched_control_fields': fields,
                      'build_controls_verified': build_verified,
                      'matched_build_fields': build_fields if build_verified else []}, indent=2))


if __name__ == '__main__':
    main()
