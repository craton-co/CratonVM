#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Self-test for `blockers.py`. Standard library only, no VM build required.

    python tools/jdk-only-blockers/selftest.py

Runs the generator over synthetic dumps that mimic **both** emitter variants
currently present in the tree (`vm/src/vm/vm_init.rs` and `vm-cli/src/main.rs`
each write a class-origin and a native census, and the two differ in their
top-level keys), then exercises determinism, the ratchet, the status ledger and
every degraded-input path.

Exits 0 when every case passes, 1 otherwise.
"""

from __future__ import annotations

import json
import os
import shutil
import sys
import tempfile

# Keep the checkout clean: importing `blockers` would otherwise drop a
# `__pycache__/` beside it. `.gitignore` covers that, but a lint job that
# checks for stray files should not have to know.
sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import blockers  # noqa: E402

FAILURES = []


def check(name, condition, detail=''):
    if condition:
        print('  ok   %s' % name)
    else:
        print('  FAIL %s %s' % (name, detail))
        FAILURES.append(name)


def write(path, obj):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, 'w', encoding='utf-8', newline='\n') as fh:
        json.dump(obj, fh, indent=2)
        fh.write('\n')


# --- fixtures --------------------------------------------------------------

# `vm-cli/src/main.rs` shape: per-kind `counts`, `real_declaring_method: null`.
REGISTRY_CLI = {
    'schema_version': 2,
    'counts': {'bridge': 1, 'intrinsic': 1, 'synthetic-stub': 2, 'total': 4},
    'natives': [
        {'class': 'java/lang/String', 'name': 'length', 'descriptor': '()I',
         'kind': 'intrinsic', 'registered_by': 'native-builtins/src/lib.rs:12',
         'overwrote': 'synthetic-stub', 'invocations': 41,
         'real_declaring_method': None},
        {'class': 'java/lang/Thread', 'name': 'start0', 'descriptor': '()V',
         'kind': 'bridge', 'registered_by': None, 'overwrote': None,
         'invocations': 2, 'real_declaring_method': None},
        # Uninvoked stub: must still be a blocker.
        {'class': 'java/util/function/Function$Identity', 'name': 'apply',
         'descriptor': '(Ljava/lang/Object;)Ljava/lang/Object;',
         'kind': 'synthetic-stub', 'registered_by': None, 'overwrote': None,
         'invocations': 0, 'real_declaring_method': None},
        {'class': 'javax/management/MBeanServer', 'name': 'queryNames',
         'descriptor': '()Ljava/util/Set;', 'kind': 'synthetic-stub',
         'registered_by': 'native-builtins/src/jmx.rs:88', 'overwrote': None,
         'invocations': 7, 'real_declaring_method': None},
    ],
}

# `vm/src/vm/vm_init.rs` shape: top-level `mode`, `real_declaring_method` object.
REGISTRY_VM = {
    'schema_version': 2,
    'mode': 'compatible',
    'counts': {'intrinsic': 1, 'bridge': 1, 'synthetic-stub': 2, 'total': 4},
    'natives': [
        dict(REGISTRY_CLI['natives'][0]),
        dict(REGISTRY_CLI['natives'][1]),
        dict(REGISTRY_CLI['natives'][2],
             real_declaring_method={'class_loaded': False,
                                    'class_is_compatibility_stub': True,
                                    'declared': False, 'has_code': False,
                                    'acc_native': False}),
        dict(REGISTRY_CLI['natives'][3],
             real_declaring_method={'class_loaded': True,
                                    'class_is_compatibility_stub': False,
                                    'declared': True, 'has_code': False,
                                    'acc_native': True}),
    ],
}

MISSING_GROUPED = {
    'version': 1,
    'modules': {
        'java.base': [
            {'class': 'jdk/internal/misc/Unsafe',
             'name': 'compareAndSetLong',
             'descriptor': '(Ljava/lang/Object;JJJ)Z',
             'sample_call_site': 'Main.main([Ljava/lang/String;)V'},
        ],
        'jdk.jfr': [
            {'class': 'jdk/jfr/internal/JVM', 'name': 'registerNatives',
             'descriptor': '()V', 'sample_call_site': None},
        ],
    },
}

MISSING_FLAT = {
    'missing_natives': [
        {'class': 'jdk/internal/misc/Unsafe', 'name': 'compareAndSetLong',
         'descriptor': '(Ljava/lang/Object;JJJ)Z',
         'sample_call_site': 'Main.main([Ljava/lang/String;)V'},
        {'class': 'sun/nio/ch/IOUtil', 'name': 'iovMax', 'descriptor': '()I',
         'sample_call_site': None},
    ],
}

CLASS_ROWS = [
    {'name': '[Ljava/lang/String;', 'origin': 'vm-array', 'reason': None,
     'requested_by': None, 'real_bytes_found': False, 'loader_id': 0},
    {'name': 'Main$$Lambda$1', 'origin': 'generated-lambda', 'reason': None,
     'requested_by': None, 'real_bytes_found': False, 'loader_id': 0},
    {'name': 'com/sun/proxy/$Proxy0', 'origin': 'generated-proxy',
     'reason': None, 'requested_by': None, 'real_bytes_found': False,
     'loader_id': 0},
    {'name': 'java/lang/Object', 'origin': 'boot-image', 'reason': None,
     'requested_by': None, 'real_bytes_found': True, 'loader_id': 0},
    {'name': 'java/util/function/Function$Identity',
     'origin': 'compatibility-stub', 'reason': 'Function.identity() stand-in',
     'requested_by': 'java/util/function/Function.identity()'
                     'Ljava/util/function/Function;',
     'real_bytes_found': False, 'loader_id': 0},
    {'name': 'org/jboss/logging/Logger', 'origin': 'compatibility-stub',
     'reason': 'enterprise prefix fallback', 'requested_by': None,
     'real_bytes_found': False, 'loader_id': 2},
]

# `vm-cli` shape.
CLASSES_CLI = {
    'schema_version': 1,
    'counts': {'boot-image': 1, 'compatibility-stub': 2, 'generated-lambda': 1,
               'generated-proxy': 1, 'vm-array': 1, 'total': 6},
    'classes': CLASS_ROWS,
}
# `vm` shape.
CLASSES_VM = {
    'schema_version': 1,
    'mode': 'compatible',
    'total': 6,
    'classes': CLASS_ROWS,
}

REPORT = {
    'schema_version': 1,
    'mode': 'jdk-only',
    'jdk_feature': 25,
    'violations': [
        {'kind': 'compatibility-class-requested', 'class': 'io/quarkus/Foo',
         'initiating_loader': None,
         'requester': 'Main.main([Ljava/lang/String;)V',
         'reason': 'enterprise prefix fallback', 'summary': 'x'},
        {'kind': 'missing-native', 'class': 'java/net/http/HttpClient',
         'method': 'sendAsync', 'descriptor': '()V',
         'module': 'java.net.http', 'summary': 'x'},
        {'kind': 'synthetic-native-registered',
         'class': 'java/util/function/Function$Identity', 'method': 'apply',
         'descriptor': '(Ljava/lang/Object;)Ljava/lang/Object;',
         'registered_by': None, 'summary': 'x'},
    ],
    'counts': {'boot_image_classes': 1, 'application_classes': 0,
               'generated_classes': 3, 'compatibility_classes': 2,
               'bridge_invocations': 2, 'intrinsic_invocations': 41,
               'synthetic_stub_invocations': 7},
}


def run(tmp, args):
    """Invoke the generator, swallowing its stderr.

    Most cases here deliberately provoke a failure, and their diagnostics would
    otherwise drown the pass/fail list. `--quiet` covers stdout only, because
    the real tool must keep shouting on stderr in CI.
    """
    saved = sys.stderr
    sys.stderr = open(os.devnull, 'w', encoding='utf-8')
    try:
        return blockers.main(args + ['--quiet'])
    finally:
        sys.stderr.close()
        sys.stderr = saved


def main():
    tmp = tempfile.mkdtemp(prefix='jdk-only-blockers-selftest-')
    try:
        d = os.path.join(tmp, 'dumps')
        write(os.path.join(d, 'registry-cli.json'), REGISTRY_CLI)
        write(os.path.join(d, 'registry-vm.json'), REGISTRY_VM)
        write(os.path.join(d, 'missing-grouped.json'), MISSING_GROUPED)
        write(os.path.join(d, 'missing-flat.json'), MISSING_FLAT)
        write(os.path.join(d, 'classes-cli.json'), CLASSES_CLI)
        write(os.path.join(d, 'classes-vm.json'), CLASSES_VM)
        write(os.path.join(d, 'report.json'), REPORT)
        with open(os.path.join(d, 'malformed.json'), 'w',
                  encoding='utf-8', newline='\n') as fh:
            fh.write('{ not json\n')

        base = os.path.join(tmp, 'baselines')
        ledger = os.path.join(tmp, 'ledger.json')

        def common(registry, classes, out):
            return ['--native-registry', os.path.join(d, registry),
                    '--missing-natives-grouped',
                    os.path.join(d, 'missing-grouped.json'),
                    '--missing-natives', os.path.join(d, 'missing-flat.json'),
                    '--class-origins', os.path.join(d, classes),
                    '--jdk-only-report', os.path.join(d, 'report.json'),
                    '--status-ledger', ledger,
                    '--baseline-dir', base,
                    '--out-dir', os.path.join(tmp, out)]

        print('generation')
        rc = run(tmp, common('registry-cli.json', 'classes-cli.json', 'cli'))
        check('vm-cli-shaped dumps generate cleanly', rc == 0, 'rc=%d' % rc)
        rc = run(tmp, common('registry-vm.json', 'classes-vm.json', 'vm'))
        check('vm-shaped dumps generate cleanly', rc == 0, 'rc=%d' % rc)

        nat_cli = os.path.join(tmp, 'cli', 'jdk-25-missing-natives.json')
        dep_cli = os.path.join(tmp, 'cli',
                               'jdk-25-synthetic-dependencies.json')
        nat = json.load(open(nat_cli, encoding='utf-8'))
        dep = json.load(open(dep_cli, encoding='utf-8'))

        check('not partial when every dump is present',
              nat['partial'] is False and dep['partial'] is False)
        check('feature version derived from the report',
              nat['jdk_feature'] == '25')
        check('both synthetic-stub registrations are entries',
              nat['counts']['by_category'][blockers.CAT_STUB_REGISTRATION] == 2)
        check('bridge and intrinsic registrations are NOT entries',
              all(e['class'] != 'java/lang/Thread' for e in nat['entries']))
        check('unresolved natives from all three sources',
              nat['counts']['by_category'][blockers.CAT_UNRESOLVED] == 4)
        check('flat and grouped rows for the same triple merge into one',
              len([e for e in nat['entries']
                   if e['class'] == 'jdk/internal/misc/Unsafe']) == 1)
        merged = [e for e in nat['entries']
                  if e['class'] == 'jdk/internal/misc/Unsafe'][0]
        check('merged entry records both sources',
              merged['sources'] == ['missing-natives',
                                    'missing-natives-grouped'],
              str(merged['sources']))
        check('module attributed from the grouped dump',
              merged['module'] == 'java.base')
        check('module derived locally for the flat-only row',
              [e for e in nat['entries']
               if e['class'] == 'sun/nio/ch/IOUtil'][0]['module'] == 'java.base')
        check('module table read from the VM source, not the fallback',
              nat['module_table_source'] == 'vm/src/vm/vm_init.rs',
              nat['module_table_source'])
        check('synthetic-stub rows request classification, never assert defect',
              all(e['requires_classification'] and
                  e['classification_reference'] == blockers.NATIVE_REVIEW_DOC
                  for e in nat['entries']
                  if e['category'] == blockers.CAT_STUB_REGISTRATION))
        check('unresolved rows do not claim an invocation count',
              all(e['invocations_known'] is False for e in nat['entries']
                  if e['category'] == blockers.CAT_UNRESOLVED))
        check('everything defaults to open',
              nat['counts']['open'] == nat['counts']['total'])
        check('registry totals passed through verbatim, nothing re-derived',
              nat['registry_totals'] == {'bridge': 1, 'intrinsic': 1,
                                         'synthetic-stub': 2, 'total': 4})

        check('compatibility-stub classes are blockers',
              dep['counts']['by_category'][blockers.CAT_STUB_CLASS] == 2)
        check('refused compatibility class is a blocker',
              dep['counts']['by_category'][blockers.CAT_CLASS_REFUSED] == 1)
        allowed_origins = sorted(a['origin'] for a in dep['allowed'])
        check('allowed generated origins reported separately',
              allowed_origins == ['generated-lambda', 'generated-proxy',
                                  'vm-array'], str(allowed_origins))
        check('allowed classes are never blockers',
              all(b['origin'] == blockers.BLOCKER_ORIGIN
                  for b in dep['blockers']))
        check('boot-image classes are neither blockers nor "generated"',
              dep['counts']['allowed_generated_total'] == 3 and
              dep['counts']['by_origin']['boot-image'] == 1)

        print('determinism')
        run(tmp, common('registry-cli.json', 'classes-cli.json', 'again'))
        for name in ('jdk-25-missing-natives.json',
                     'jdk-25-synthetic-dependencies.json'):
            a = open(os.path.join(tmp, 'cli', name), 'rb').read()
            b = open(os.path.join(tmp, 'again', name), 'rb').read()
            check('%s is byte-identical across runs' % name, a == b)
            check('%s has no CRLF' % name, b'\r\n' not in a)
            check('%s contains no absolute path' % name,
                  b':\\' not in a and b'/tmp/' not in a and
                  tmp.encode().replace(b'\\', b'/') not in a.replace(b'\\\\',
                                                                    b'/'))

        print('ratchet')
        rc = run(tmp, common('registry-cli.json', 'classes-cli.json', 'cli')
                 + ['--check'])
        check('--check without a baseline is a configuration error', rc == 2,
              'rc=%d' % rc)
        rc = run(tmp, common('registry-cli.json', 'classes-cli.json', 'cli')
                 + ['--update-baseline'])
        check('--update-baseline writes both files', rc == 0 and
              os.path.isfile(os.path.join(base,
                                          'jdk-25-missing-natives.json')))
        rc = run(tmp, common('registry-cli.json', 'classes-cli.json', 'cli')
                 + ['--check'])
        check('--check passes against a fresh baseline', rc == 0,
              'rc=%d' % rc)

        # Growth by an entry that was NEVER invoked must still fail.
        grown = json.loads(json.dumps(REGISTRY_CLI))
        grown['natives'].append(
            {'class': 'java/lang/ProcessHandle', 'name': 'current',
             'descriptor': '()Ljava/lang/ProcessHandle;',
             'kind': 'synthetic-stub', 'registered_by': None,
             'overwrote': None, 'invocations': 0,
             'real_declaring_method': None})
        write(os.path.join(d, 'registry-grown.json'), grown)
        rc = run(tmp, common('registry-grown.json', 'classes-cli.json',
                             'grown') + ['--check'])
        check('a new UNINVOKED stub fails the ratchet', rc == 1, 'rc=%d' % rc)

        grown_classes = json.loads(json.dumps(CLASSES_CLI))
        grown_classes['classes'].append(
            {'name': 'io/smallrye/Bar', 'origin': 'compatibility-stub',
             'reason': 'enterprise prefix fallback', 'requested_by': None,
             'real_bytes_found': False, 'loader_id': 3})
        write(os.path.join(d, 'classes-grown.json'), grown_classes)
        rc = run(tmp, common('registry-cli.json', 'classes-grown.json',
                             'grown2') + ['--check'])
        check('a new compatibility-stub class fails the ratchet', rc == 1,
              'rc=%d' % rc)

        # An extra ALLOWED generated class must not fail the ratchet.
        extra_allowed = json.loads(json.dumps(CLASSES_CLI))
        extra_allowed['classes'].append(
            {'name': 'Main$$Lambda$2', 'origin': 'generated-lambda',
             'reason': None, 'requested_by': None, 'real_bytes_found': False,
             'loader_id': 0})
        write(os.path.join(d, 'classes-allowed.json'), extra_allowed)
        rc = run(tmp, common('registry-cli.json', 'classes-allowed.json',
                             'allowed') + ['--check'])
        check('an extra allowed generated class does NOT fail the ratchet',
              rc == 0, 'rc=%d' % rc)

        print('status ledger')
        write(ledger, {
            'schema_version': 1,
            'natives': [
                {'class': 'javax/management/MBeanServer',
                 'name': 'queryNames', 'descriptor': '()Ljava/util/Set;',
                 'status': 'bridge',
                 'note': 'reviewed: permanent bridge, mis-tagged by the '
                         'ambient default category'},
                {'class': 'java/lang/Nope', 'name': 'x', 'descriptor': '()V',
                 'status': 'out-of-scope', 'note': 'stale row'},
            ],
            'classes': [
                {'name': 'java/util/function/Function$Identity',
                 'status': 'real-bytecode', 'note': 'real bytes load now'},
            ],
        })
        rc = run(tmp, common('registry-cli.json', 'classes-cli.json',
                             'closed') + ['--check'])
        check('closing entries shrinks the open set and still passes', rc == 0,
              'rc=%d' % rc)
        closed = json.load(open(os.path.join(tmp, 'closed',
                                             'jdk-25-missing-natives.json'),
                                encoding='utf-8'))
        check('closed entry carries its status and note',
              closed['counts']['closed'] == 1 and
              closed['counts']['by_status'].get('bridge') == 1)
        check('a stale ledger row is reported, not silently accepted',
              any('matched nothing' in n for n in closed['notes']))

        bad = os.path.join(tmp, 'bad-ledger.json')
        write(bad, {'natives': [{'class': 'a', 'name': 'b',
                                 'descriptor': '()V',
                                 'status': 'probably-fine'}]})
        rc = run(tmp, common('registry-cli.json', 'classes-cli.json', 'bad')
                 + ['--status-ledger', bad])
        check('an off-vocabulary status is a hard error', rc == 2,
              'rc=%d' % rc)

        print('degraded inputs')
        rc = run(tmp, ['--native-registry', os.path.join(d, 'nonexistent.json'),
                       '--class-origins', os.path.join(d, 'classes-cli.json'),
                       '--jdk-only-report', os.path.join(d, 'report.json'),
                       '--status-ledger', ledger, '--baseline-dir', base,
                       '--out-dir', os.path.join(tmp, 'partial')])
        check('a missing dump still generates a partial result', rc == 0,
              'rc=%d' % rc)
        partial = json.load(open(os.path.join(tmp, 'partial',
                                              'jdk-25-missing-natives.json'),
                                 encoding='utf-8'))
        check('partial result is flagged', partial['partial'] is True)
        check('partial result names the missing input',
              partial['inputs']['native-registry'] == 'missing')
        check('partial result carries an explicit note',
              any('NOT evidence of a clean run' in n
                  for n in partial['notes']))
        check('partial result is not a fabricated zero',
              partial['counts']['total'] > 0)

        rc = run(tmp, ['--native-registry', os.path.join(d, 'nonexistent.json'),
                       '--class-origins', os.path.join(d, 'classes-cli.json'),
                       '--jdk-only-report', os.path.join(d, 'report.json'),
                       '--status-ledger', ledger, '--baseline-dir', base,
                       '--out-dir', os.path.join(tmp, 'partial'), '--check'])
        check('--check refuses a partial result', rc == 3, 'rc=%d' % rc)
        rc = run(tmp, ['--native-registry', os.path.join(d, 'nonexistent.json'),
                       '--class-origins', os.path.join(d, 'classes-cli.json'),
                       '--jdk-only-report', os.path.join(d, 'report.json'),
                       '--status-ledger', ledger, '--baseline-dir', base,
                       '--out-dir', os.path.join(tmp, 'partial'),
                       '--update-baseline'])
        check('--update-baseline refuses a partial result', rc == 3,
              'rc=%d' % rc)

        rc = run(tmp, ['--native-registry', os.path.join(d, 'malformed.json'),
                       '--class-origins', os.path.join(d, 'classes-cli.json'),
                       '--jdk-feature', '21',
                       '--out-dir', os.path.join(tmp, 'malformed')])
        check('a malformed dump degrades instead of crashing', rc == 0,
              'rc=%d' % rc)
        mal = json.load(open(os.path.join(tmp, 'malformed',
                                          'jdk-21-missing-natives.json'),
                             encoding='utf-8'))
        check('malformed input is labelled malformed',
              mal['inputs']['native-registry'] == 'malformed')
        check('explicit --jdk-feature keys the output file',
              mal['jdk_feature'] == '21')

        rc = run(tmp, ['--native-registry', os.path.join(d, 'nonexistent.json'),
                       '--jdk-feature', '25',
                       '--out-dir', os.path.join(tmp, 'nothing')])
        check('no readable input at all writes nothing and fails', rc == 3,
              'rc=%d' % rc)
        check('no output file was fabricated',
              not os.path.isdir(os.path.join(tmp, 'nothing')))

        rc = run(tmp, ['--native-registry',
                       os.path.join(d, 'registry-cli.json'),
                       '--out-dir', os.path.join(tmp, 'nofeature')])
        check('an undeterminable feature version is a hard error', rc == 2,
              'rc=%d' % rc)

        write(os.path.join(d, 'unknown-origin.json'), {
            'schema_version': 1,
            'classes': [{'name': 'X', 'origin': 'brand-new-origin',
                         'reason': None, 'requested_by': None,
                         'real_bytes_found': False, 'loader_id': 0}],
        })
        run(tmp, ['--class-origins', os.path.join(d, 'unknown-origin.json'),
                  '--jdk-feature', '25',
                  '--out-dir', os.path.join(tmp, 'unknown')])
        unk = json.load(open(os.path.join(
            tmp, 'unknown', 'jdk-25-synthetic-dependencies.json'),
            encoding='utf-8'))
        check('an unrecognised origin is named, not silently allowed',
              unk['partial'] is True and
              any('brand-new-origin' in n for n in unk['notes']) and
              not unk['allowed'])
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    print()
    if FAILURES:
        print('%d FAILURE(S): %s' % (len(FAILURES), ', '.join(FAILURES)))
        return 1
    print('all checks passed')
    return 0


if __name__ == '__main__':
    sys.exit(main())
