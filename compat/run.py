#!/usr/bin/env python3
"""Run the Switchboard compatibility suite against the switchboard binary.

  python3 -B compat/run.py --bin path/to/switchboard
  python3 -B compat/run.py --bin path/to/switchboard -k ping

Every component is started from the binary (`<binary> server`, `bridge`,
`helper`, `cli`), or from a --<component>-cmd override, and treated as a black
box. Standard library only.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

COMPAT = Path(__file__).resolve().parent
sys.path.insert(0, str(COMPAT))
sys.dont_write_bytecode = True


def worker(module, pattern, verbosity):
    """Run one test module in this process; print a one-line JSON summary last."""
    from sbcompat.impl import IMPL  # noqa: F401  (validates the configuration early)
    loader = unittest.TestLoader()
    if pattern:
        loader.testNamePatterns = [f'*{p}*' for p in pattern]
    suite = loader.loadTestsFromName('tests.' + module)
    result = unittest.TextTestRunner(stream=sys.stdout, verbosity=verbosity).run(suite)
    summary = dict(module=module, run=result.testsRun, failures=len(result.failures), errors=len(result.errors),
                   skipped=len(result.skipped), expected_failures=len(result.expectedFailures),
                   unexpected_successes=len(result.unexpectedSuccesses))
    print('SBCOMPAT-SUMMARY ' + json.dumps(summary), flush=True)
    return 0 if result.wasSuccessful() else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--bin', help='the switchboard binary under test (default: target/release/switchboard)')
    for component in ('server', 'bridge', 'helper', 'cli'):
        parser.add_argument(f'--{component}-cmd', help=f'override the {component} command (shell words)')
    parser.add_argument('-k', action='append', dest='pattern', help='only tests whose name contains this (repeatable)')
    parser.add_argument('-j', '--jobs', type=int, default=4, help='test modules run in parallel (default 4)')
    parser.add_argument('--quick', action='store_true', help='skip tests that wait on 30-second protocol timers')
    parser.add_argument('-v', '--verbose', action='store_true')
    parser.add_argument('--module', help='run only this test module (e.g. test_server_api)')
    parser.add_argument('--worker', help=argparse.SUPPRESS)
    args = parser.parse_args()
    verbosity = 2 if args.verbose else 1
    if args.worker:
        return worker(args.worker, args.pattern, verbosity)

    from sbcompat.impl import resolve
    overrides = {c: getattr(args, c + '_cmd') for c in ('server', 'bridge', 'helper', 'cli')}
    impl = resolve(args.bin, overrides=overrides)
    if not all(overrides.values()) and not os.access(impl['binary'], os.X_OK):
        parser.error(f"no executable binary at {impl['binary']} (build it with cargo build --release, or pass --bin)")
    env = dict(os.environ, SBCOMPAT_IMPL=json.dumps(impl), PYTHONDONTWRITEBYTECODE='1',
               SBCOMPAT_QUICK='1' if args.quick else '0')
    modules = sorted(p.stem for p in (COMPAT / 'tests').glob('test_*.py'))
    if args.module:
        modules = [m for m in modules if m == args.module or m == 'test_' + args.module]
        if not modules:
            parser.error('no such module: ' + args.module)
    print(f"Binary: {impl['binary']}")
    for component in ('server', 'bridge', 'helper', 'cli'):
        print(f"  {component}: {' '.join(impl[component])}")
    started = time.monotonic()
    running, results, exit_code = [], [], 0
    queue = list(modules)
    while queue or running:
        while queue and len(running) < max(1, args.jobs):
            module = queue.pop(0)
            command = [sys.executable, '-B', str(COMPAT / 'run.py'), '--worker', module]
            command += ['-v'] if args.verbose else []
            for pattern in args.pattern or []:
                command += ['-k', pattern]
            log = tempfile.TemporaryFile('w+')
            process = subprocess.Popen(command, cwd=COMPAT, env=env, stdout=log, stderr=subprocess.STDOUT, text=True)
            process.log = log
            running.append((module, process, time.monotonic()))
        for item in list(running):
            module, process, began = item
            if process.poll() is None:
                continue
            running.remove(item)
            process.log.seek(0)
            output = process.log.read()
            process.log.close()
            summary = None
            lines = []
            for line in output.splitlines():
                if line.startswith('SBCOMPAT-SUMMARY '):
                    summary = json.loads(line.split(' ', 1)[1])
                else:
                    lines.append(line)
            print(f'\n=== {module} ({time.monotonic() - began:.1f}s) ===')
            print('\n'.join(lines))
            if summary is None:
                summary = dict(module=module, run=0, failures=0, errors=1, skipped=0, expected_failures=0,
                               unexpected_successes=0)
            results.append(summary)
            if process.returncode:
                exit_code = 1
        time.sleep(0.05)
    print('\n=== Summary ===')
    totals = {key: sum(r[key] for r in results) for key in
              ('run', 'failures', 'errors', 'skipped', 'expected_failures', 'unexpected_successes')}
    for r in sorted(results, key=lambda r: r['module']):
        print(f"  {r['module']:<28} run {r['run']:>3}  fail {r['failures']}  error {r['errors']}  "
              f"skip {r['skipped']}  xfail {r['expected_failures']}  xpass {r['unexpected_successes']}")
    print(f"  {'total':<28} run {totals['run']:>3}  fail {totals['failures']}  error {totals['errors']}  "
          f"skip {totals['skipped']}  xfail {totals['expected_failures']}  xpass {totals['unexpected_successes']}")
    print(f"{'PASS' if exit_code == 0 else 'FAIL'} in {time.monotonic() - started:.1f}s")
    return exit_code


if __name__ == '__main__':
    sys.exit(main())
