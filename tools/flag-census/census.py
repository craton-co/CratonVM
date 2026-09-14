#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Regenerate the `CRATONVM_*` environment-flag census.

Usage:  python3 tools/flag-census/census.py [REPO_ROOT] [OUT_MD]

Defaults to the repo root inferred from this file's location and
`flag-census.md`.

The scan is deliberately *literal-only*: it looks for `"CRATONVM_..."` string
literals in Rust code (outside `//` comments) and for bare `CRATONVM_...`
tokens everywhere else.  That is exhaustive because the workspace contains no
dynamic env-var name construction -- see the assertion printed at the end of a
run, which fails loudly if a `format!("CRATONVM_{...}")` ever appears.

Sections and per-flag classification are derived mechanically; the two curated
tables (documented-but-dead highlights, and the boolean truth tables) live in
`_HIGHLIGHTS` / `_TRUTH_TABLES` below and must be maintained by hand.
"""

import collections
import os
import re
import sys

NAME_LITERAL = re.compile(r'"(CRATONVM_[A-Z0-9_]+)"')
NAME_BARE = re.compile(r'CRATONVM_[A-Z0-9_]+')
DYNAMIC_NAME = re.compile(r'"CRATONVM_[A-Z0-9_]*\{')

SKIP_DIRS = {'target', '.git', 'jdk25src', 'node_modules'}
# The census output and the scanner itself both mention every flag name; scanning
# them would make the totals self-referential and drift on every regeneration.
SKIP_FILES = {'flag-census.md', 'tools/flag-census/census.py',
              'tools/flag-census/render.py'}

# The typed configuration itself. Its sites ARE read sites — they are where the
# parse now happens — so they must be scanned or every migrated flag would be
# misclassified as dead. But they are not *consumer* sites, so they are tagged
# and excluded from the "still reads the environment directly" metrics.
CONFIG_FILE = 'types/src/flags.rs'
TEXT_EXT = {'.md', '.sh', '.py', '.java', '.toml', '.yml', '.yaml', '.txt',
            '.ps1', '.cmd', '.bat', '.json', '.xml', ''}

# `CRATONVM_H` is the C header include guard; `CRATONVM_JNI_*` are JNI return
# constants re-exported by `libcratonvm/include/cratonvm.h`.  Neither is an
# environment variable.
BOGUS = {'CRATONVM_H'}

DEBUG_TOKENS = ('DBG', 'DEBUG', 'TRACE', 'DIAG', 'VERBOSE', 'DUMP', 'LOG',
                'STATS', 'PROBE', 'AUDIT', 'PRINT', 'REPORT', 'SPEW')
TEST_PREFIXES = ('CRATONVM_TEST_', 'CRATONVM_SOAK_', 'CRATONVM_NONEXISTENT',
                 'CRATONVM_REGEN_HEADER', 'CRATONVM_JAVA_HOME',
                 'CRATONVM_TEST_JDK')
# Flags whose name does not contain a debug token but which only gate extra
# assertions / verification passes / capture buffers.
DIAG_EXTRA = {
    'CRATONVM_GC_ARRAY_GUARD_BT', 'CRATONVM_SP_VERIFY',
    'CRATONVM_MOVING_YOUNG_VERIFY', 'CRATONVM_DEOPT_VERIFY',
    'CRATONVM_SHADOW_WATCH', 'CRATONVM_SYMBOLIZE', 'CRATONVM_LOCK_ORDER_CHECK',
    'CRATONVM_ASSERT_SINGLE_OS_THREAD', 'CRATONVM_GC_VERIFY_STALE',
    'CRATONVM_SOCKET_CAPTURE', 'CRATONVM_TRACK_NATIVE',
    'CRATONVM_JIT_BISECT_ONLY', 'CRATONVM_JIT_BISECT_SKIP',
    'CRATONVM_SHADOW_SENTINEL', 'CRATONVM_FWD_RESOLVE_STRICT',
    'CRATONVM_LONGROOT_STRICT', 'CRATONVM_STRICT_SWALLOWS',
    'CRATONVM_ENABLE_ASSERTIONS', 'CRATONVM_KEEP_SCRIPT',
}

DEFERRED_CRATE = 'native-builtins'


def strip_line_comment(line):
    """Return `line` with any `//` comment removed, respecting string literals."""
    out = []
    i = 0
    in_string = False
    while i < len(line):
        c = line[i]
        if in_string:
            if c == '\\':
                out.append(c)
                i += 2
                continue
            if c == '"':
                in_string = False
        else:
            if c == '"':
                in_string = True
            elif c == '/' and i + 1 < len(line) and line[i + 1] == '/':
                return ''.join(out)
        out.append(c)
        i += 1
    return ''.join(out)


def scan(root):
    """Return (rust_sites, nonrust_refs)."""
    sites = []
    nonrust = collections.defaultdict(collections.Counter)
    dynamic = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for fn in filenames:
            path = os.path.join(dirpath, fn)
            rel = os.path.relpath(path, root).replace(os.sep, '/')
            if rel in SKIP_FILES:
                continue
            if fn.endswith('.rs'):
                text = _read(path)
                if text is None or 'CRATONVM_' not in text:
                    continue
                if DYNAMIC_NAME.search(text):
                    dynamic.append(rel)
                crate = rel.split('/')[0]
                lines = text.split('\n')
                for idx, line in enumerate(lines):
                    code = strip_line_comment(line)
                    names = set(NAME_LITERAL.findall(code))
                    if not names:
                        continue
                    ctx = '\n'.join(lines[max(0, idx - 5):idx + 6])
                    cached = any(tok in ctx for tok in
                                 ('OnceLock', 'once_cell', 'lazy_static',
                                  'LazyLock', 'get_or_init'))
                    for name in names:
                        sites.append({
                            'crate': crate, 'file': rel, 'line': idx + 1,
                            'name': name, 'kind': _kind(code, name),
                            'cached': cached,
                            'test': '/tests/' in rel or '/benches/' in rel
                                    or rel.startswith('difftest/'),
                            'text': line.strip()[:200],
                            'config': rel == CONFIG_FILE,
                            # A literal `std::env::var` / `var_os` call on this
                            # line: the thing the migration removes. A name
                            # that merely appears in an assertion message or as
                            # a label argument is not one.
                            'direct': 'env::var' in code and rel != CONFIG_FILE,
                        })
                continue
            if rel.startswith('libcratonvm/include') or fn == 'cbindgen.toml':
                continue
            if os.path.splitext(fn)[1] not in TEXT_EXT:
                continue
            text = _read(path)
            if text is None or 'CRATONVM_' not in text:
                continue
            top = rel.split('/')[0]
            for name in NAME_BARE.findall(text):
                nonrust[name][top] += 1
    if dynamic:
        raise SystemExit('dynamic CRATONVM_ name construction found in %s -- the '
                         'literal-only scan is no longer exhaustive' % dynamic)
    return sites, nonrust


def _read(path):
    try:
        if os.path.getsize(path) > 4_000_000:
            return None
        with open(path, encoding='utf-8', errors='replace') as fh:
            return fh.read()
    except OSError:
        return None


def _kind(code, name):
    q = '"' + name + '"'
    if re.search(r'var_os\s*\(\s*' + re.escape(q), code):
        return 'read'
    if re.search(r'\bvar\s*\(\s*' + re.escape(q), code):
        return 'read'
    if re.search(r'option_env!\s*\(\s*' + re.escape(q), code):
        return 'option_env'
    if re.search(r'set_var\s*\(\s*' + re.escape(q), code) or \
       re.search(r'\.env\s*\(\s*' + re.escape(q), code):
        return 'set'
    if re.search(r'remove_var\s*\(\s*' + re.escape(q), code) or \
       re.search(r'env_remove\s*\(\s*' + re.escape(q), code):
        return 'unset'
    # Everything else is an indirect read: a macro invocation
    # (`cached_is_set!(f, "X")`), a helper call (`num("X")`), a table entry, or
    # a `const NAME: &str` binding.  All of them feed a read.
    return 'read'


def is_bogus(name, has_literal):
    if name in BOGUS or name.endswith('_'):
        return True
    return name.startswith('CRATONVM_JNI_') and not has_literal


def aggregate(sites, nonrust):
    by_name = collections.defaultdict(list)
    for s in sites:
        by_name[s['name']].append(s)
    out = []
    for name in sorted(set(by_name) | set(nonrust)):
        if is_bogus(name, name in by_name):
            continue
        own = by_name.get(name, [])
        reads = [s for s in own if s['kind'] == 'read']
        prod = [s for s in reads if not s['test']]
        nb_reads = [s for s in reads if s['crate'] == DEFERRED_CRATE]
        consumer = [s for s in reads if not s['config']]
        if not reads:
            klass = 'd-dead'
        elif not prod:
            klass = 'c-test'
        elif any(t in name for t in DEBUG_TOKENS):
            klass = 'a-diag'
        elif any(name.startswith(t) for t in TEST_PREFIXES):
            klass = 'c-test'
        elif name in DIAG_EXTRA:
            klass = 'a-diag'
        else:
            klass = 'b-semantics'
        out.append({
            'name': name, 'klass': klass, 'reads': len(reads),
            'consumer_reads': len(consumer),
            'migrated': bool(reads) and not consumer,
            'cached': sum(1 for s in consumer if s['cached']),
            'uncached': sum(1 for s in consumer if not s['cached']),
            'nb_reads': len(nb_reads),
            'nb_only': bool(consumer) and all(
                s['crate'] == DEFERRED_CRATE for s in consumer),
            'nb_direct': sum(1 for s in own
                             if s['crate'] == DEFERRED_CRATE and s['direct']),
            'crates': ','.join(sorted({s['crate'] for s in reads})),
            'setters': sum(1 for s in own if s['kind'] in ('set', 'unset')),
            'nonrust': ','.join('%s:%d' % kv for kv in
                                sorted(nonrust.get(name, {}).items())),
            'sample': '%s:%d' % (reads[0]['file'], reads[0]['line']) if reads else '',
            'polarity': _polarity(name, reads),
        })
    return out


def _polarity(name, reads):
    for s in reads:
        t = s['text']
        if re.search(r'var_os\("%s"\)\.is_none\(\)' % name, t) or \
           re.search(r'var\("%s"\)\.is_err\(\)' % name, t):
            return 'opt-out (default ON)'
        if re.search(r'var_os\("%s"\)\.is_some\(\)' % name, t) or \
           re.search(r'var\("%s"\)\.is_ok\(\)' % name, t):
            return 'opt-in (default OFF)'
    return 'value/other' if reads else '-'


def main():
    root = sys.argv[1] if len(sys.argv) > 1 else \
        os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    sites, nonrust = scan(root)
    names = aggregate(sites, nonrust)
    counts = collections.Counter(n['klass'] for n in names)
    print('rust literal sites : %d' % len(sites))
    print('read sites         : %d (%d outside %s)' % (
        sum(n['reads'] for n in names),
        sum(n['reads'] - n['nb_reads'] for n in names), DEFERRED_CRATE))
    print('consumer reads     : %d (still call env::var directly)'
          % sum(n['consumer_reads'] for n in names))
    print('flags fully migrated onto VmFlags: %d'
          % sum(1 for n in names if n['migrated']))
    print('uncached consumer reads : %d' % sum(n['uncached'] for n in names))
    print('distinct flags     : %d' % len(names))
    for k in ('a-diag', 'b-semantics', 'c-test', 'd-dead'):
        print('  %-12s %d' % (k, counts[k]))


if __name__ == '__main__':
    main()
