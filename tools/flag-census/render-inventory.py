#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
"""Regenerate the Full inventory table in `docs/config/flag-inventory.md`.

Usage:  python3 tools/flag-census/render-inventory.py [REPO_ROOT]

That table has always said "generated, not maintained" without a generator
behind it, and by 2026-08-04 it had drifted 42 rows behind `INVENTORY` while
claiming a declared count of 647 against a true 689. Nothing failed. This
script is the missing half; `types/tests/flag_docs_generated.rs` is the other
half, and fails `cargo test` when the checked-in table and the code disagree.

Inputs, exactly the three the document's own "How to regenerate" section names:

  * `types/src/flag_groups.rs::INVENTORY`  — group, token, on_key/off_key/off_word
  * `types/tests/flag-surface.txt`         — the declared set (cross-checked)
  * a scan of `<crate>/src/**/*.rs`        — the *Read in* column

Everything from the `## Full inventory` heading to EOF is replaced, plus the
two count claims earlier in the file. The prose above is left alone.

The eleven allowlisted rows carry hand-written *Read in* text (they name a test
harness, not a crate), so they live in `ALLOWLISTED` below and must be kept in
step with `ALLOWED` in `types/tests/flag_declaration_guard.rs` — which
`flag_docs_generated.rs` also checks.
"""

import os
import re
import sys

ROOT = sys.argv[1] if len(sys.argv) > 1 else \
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

GROUPS_RS = os.path.join(ROOT, 'types', 'src', 'flag_groups.rs')
FIXTURE = os.path.join(ROOT, 'types', 'tests', 'flag-surface.txt')
OUT = os.path.join(ROOT, 'docs', 'config', 'flag-inventory.md')

# Undeclared-but-exempt names. `Read in` is prose here because these are read
# by a harness or are not variables at all; the rest of the row is fixed.
# Keyed in the same order the table sorts, i.e. by name.
ALLOWLISTED = {
    'CRATONVM_COMPATIBILITY_JDK_ONLY': 'libcratonvm C ABI constant, not a variable',
    'CRATONVM_DIFF_HOTSPOT': 'vm/tests differential harness',
    'CRATONVM_FUZZ_BOOTCP': 'cargo-fuzz target',
    'CRATONVM_NONEXISTENT_VAR_12345': 'absent-name probe',
    'CRATONVM_REAL_RAF': 'retired gate; env_remove baseline only',
    'CRATONVM_REGEN_HEADER': 'libcratonvm/build.rs',
    'CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS': 'vm/tests opt-in',
    'CRATONVM_SOMETHING_BRAND_NEW': 'unknown-key fall-through probe',
    'CRATONVM_SPRING_BOOT_FATJAR': 'vm/tests fixture path',
    'CRATONVM_TEST_CLASSES_DIR': 'vm/build.rs, read with option_env!',
    'CRATONVM_TEST_JAVA_HOME': 'vm/tests JDK location',
}

ENTRY_RE = re.compile(
    r'E \{ group: Group::([A-Z]+), token: "([a-z0-9-]+)", '
    r'on_key: (?:Some\("([A-Z_0-9]+)"\)|None), '
    r'off_key: (?:Some\("([A-Z_0-9]+)"\)|None), '
    r'off_word: (?:Some\("([^"]*)"\)|None)'
)
GROUP_VAR_RE = re.compile(r'Group::([A-Z]+) => "(CRATONVM_[A-Z]+)"')
SCALARS_RE = re.compile(r'pub const SCALARS: &\[&str\] = &\[(.*?)\];', re.S)
# An exact whole-string literal: the closing quote follows the name at once.
# Same precision rule as `flag_declaration_guard.rs::exact_literals`.
LITERAL_RE = re.compile(r'"(CRATONVM_[A-Z_0-9]*)"')


def parse_inventory(src):
    """[(group, token, on_key, off_key, off_word)] in declaration order."""
    return [(m.group(1), m.group(2), m.group(3), m.group(4), m.group(5))
            for m in ENTRY_RE.finditer(src)]


def crate_dirs(root):
    for name in sorted(os.listdir(root)):
        if os.path.isdir(os.path.join(root, name, 'src')):
            yield name


def read_sites(root):
    """{VARIABLE: {crate, ...}} over `<crate>/src/**/*.rs`.

    `types/src/flag_groups.rs` is excluded: the registry names every variable,
    and that is a declaration, not a read. Whole-line comments are skipped so
    the tree's hundreds of `/// CRATONVM_X=1 does …` lines cost nothing.
    """
    sites = {}
    for crate in crate_dirs(root):
        for dirpath, dirnames, filenames in os.walk(os.path.join(root, crate, 'src')):
            dirnames[:] = [d for d in dirnames if d not in ('target', '.git')]
            for fn in filenames:
                if not fn.endswith('.rs'):
                    continue
                path = os.path.join(dirpath, fn)
                if os.path.abspath(path) == os.path.abspath(GROUPS_RS):
                    continue
                with open(path, encoding='utf-8', errors='replace') as fh:
                    for line in fh:
                        if line.lstrip().startswith('//'):
                            continue
                        for name in LITERAL_RE.findall(line):
                            sites.setdefault(name, set()).add(crate)
    return sites


def rows(root):
    src = open(GROUPS_RS, encoding='utf-8').read()
    inventory = parse_inventory(src)
    group_var = dict((g, v) for g, v in GROUP_VAR_RE.findall(src))
    scalars = re.findall(r'"(CRATONVM_[A-Z_0-9]+)"', SCALARS_RE.search(src).group(1))
    sites = read_sites(root)

    out = {}

    def read_in(name):
        return ', '.join(sorted(sites.get(name, ()))) or '—'

    for group, token, on_key, off_key, off_word in inventory:
        # Shape and default, per the document's derivation rules. The default
        # is the CANONICAL TOKEN's, which is always stated positively: a knob
        # spelled only `CRATONVM_X_NO_Y` is `opt-out` / `on`.
        if on_key and off_key:
            shape, default = 'both', 'off'
        elif off_key:
            shape, default = 'opt-out', 'on'
        elif off_word is not None:
            shape, default = 'default-on', 'on'
        else:
            shape, default = 'opt-in', 'off'
        canonical = '`%s=%s`' % (group_var[group], token)
        cls = 'diag' if group == 'DBG' else 'behaviour'
        for key in (on_key, off_key):
            if key:
                out[key] = (group, canonical, shape, default, cls,
                            'snapshot', read_in(key))

    for name in scalars:
        out[name] = ('—', '`%s`' % name, 'scalar', 'unset', 'behaviour',
                     'snapshot', read_in(name))

    for group, var in group_var.items():
        out[var] = (group, '`%s=…`' % var, 'group', 'unset', '—',
                    'snapshot', read_in(var))

    declared = set(out)

    for name, why in ALLOWLISTED.items():
        out[name] = ('—', 'n/a (undeclared)', 'live', 'unset', 'harness/ABI',
                     'live getenv', why)

    return out, declared


# The two "Where the surface stands" rows this file did NOT used to write.
#
# They are hand-maintained numbers over a scan of the tree, so they went stale
# every time anyone added a flag anywhere — `flag_inventory_surface_counts_are_
# current` caught them drifting by 350 once, and by 1 four times in the two days
# before this was written, each time from an unrelated branch. A number that
# only a human can refresh, in a file a generator rewrites, is a red build
# waiting for the next commit. Now the generator writes them.
#
# These reimplement `types/tests/doc_numeric_claims.rs`'s `collect_rust`,
# `scan_identifiers` and `scan_literals` EXACTLY, because that test is what
# enforces the result: same skipped directories (including `docs/internal`,
# which the shell recipe in the test's failure message does not exclude), same
# "prefix plus at least one [A-Z0-9_]" rule, same members-with-a-src-dir walk
# for row 2, and the same silent skip of a file that is not valid UTF-8.
SKIPPED_DIRS = {'target', '.git', 'vendor', 'node_modules'}
IDENT_RE = re.compile(r'CRATONVM_[A-Z0-9_]+')
LITERAL_RE = re.compile(r'"(CRATONVM_[A-Z0-9_]+)"')


def _rust_files(root, start):
    """Every `*.rs` under `start`, with the test's exclusions."""
    docs_internal = os.path.join(root, 'docs', 'internal')
    found = []
    for dirpath, dirnames, filenames in os.walk(start):
        dirnames[:] = [
            d for d in dirnames
            if d not in SKIPPED_DIRS
            and not d.startswith('.')
            and os.path.join(dirpath, d) != docs_internal
        ]
        found += [os.path.join(dirpath, f) for f in filenames if f.endswith('.rs')]
    return found


def _read_utf8(path):
    """The test uses `read_to_string`, which SKIPS a non-UTF-8 file rather than
    lossily decoding it. Matching that matters: a lossy decode could invent or
    destroy a name and move the count by one with nothing to point at."""
    try:
        with open(path, encoding='utf-8') as fh:
            return fh.read()
    except (OSError, UnicodeDecodeError):
        return None


def surface_counts(root):
    """`(row 1, row 2)` of the "Where the surface stands" table."""
    identifiers = set()
    for path in _rust_files(root, root):
        text = _read_utf8(path)
        if text is not None:
            identifiers.update(IDENT_RE.findall(text))

    literals = set()
    for member in _workspace_members(root):
        src = os.path.join(root, member, 'src')
        if not os.path.isdir(src):
            continue
        for path in _rust_files(root, src):
            text = _read_utf8(path)
            if text is not None:
                literals.update(LITERAL_RE.findall(text))

    return len(identifiers), len(literals)


def _workspace_members(root):
    """The `members = [...]` list, the way the test reads it."""
    with open(os.path.join(root, 'Cargo.toml'), encoding='utf-8') as fh:
        manifest = fh.read()
    open_at = manifest.index('members = [')
    rest = manifest[open_at + len('members = ['):]
    return [
        m.strip().strip('"')
        for m in rest[:rest.index(']')].split(',')
        if m.strip().strip('"')
    ]


def main():
    table, declared = rows(ROOT)

    fixture = set(l.strip() for l in open(FIXTURE, encoding='utf-8')
                  if l.strip() and not l.startswith('#'))
    if fixture != declared:
        # The Rust test owns this agreement; failing here too means a bad table
        # can never be written in the first place.
        missing = sorted(declared - fixture)
        extra = sorted(fixture - declared)
        sys.exit('flag-surface.txt disagrees with INVENTORY; run '
                 '`cargo test -p cratonvm-types --test flag_surface` first.\n'
                 '  only in INVENTORY: %s\n  only in fixture: %s'
                 % (missing, extra))

    body = ['| Variable | Group | Canonical spelling | Shape | Default | '
            'Class | Latched | Read in |',
            '|---|---|---|---|---|---|---|---|']
    for name in sorted(table):
        body.append('| `%s` | %s |' % (name, ' | '.join(table[name])))

    header = ('## Full inventory\n\n'
              '%d rows: %d declared, %d allowlisted. Generated by\n'
              '`tools/flag-census/render-inventory.py` — see\n'
              '[How to regenerate](#how-to-regenerate).\n\n'
              % (len(table), len(declared), len(ALLOWLISTED)))

    doc = open(OUT, encoding='utf-8').read()
    head = doc.split('## Full inventory')[0]
    # The head is the hand-written prose above `## Full inventory`, and it is
    # carried through VERBATIM. That is what let a merge conflict survive
    # regeneration: the markers sat in the head, so every rerun copied them
    # forward, and the `re.sub` below has no `count=`, so it rewrote the row on
    # BOTH sides of the conflict to the same number and made the conflict look
    # vacuous. Refuse instead of laundering them — for a conflict that is NOT
    # vacuous, silently dropping the markers would pick a side at random.
    for marker in ('<<<<<<< ', '\n=======\n', '>>>>>>> '):
        if marker in head:
            sys.exit(
                '%s has unresolved merge-conflict markers above '
                '"## Full inventory".\n'
                '  That region is preserved verbatim, so regenerating would '
                'copy them forward.\n'
                '  Resolve the conflict in the file first, then rerun.'
                % OUT)
    head = re.sub(
        r'(\| \*\*declared\*\* in `flag_groups::INVENTORY` \+ scalars \+ '
        r'group variables \| \*\*)\d+(\*\* \|)',
        r'\g<1>%d\g<2>' % len(declared), head)
    # Rows 1 and 2 of the same table, which used to be hand-maintained. Written
    # with thousands separators because that is the spelling already in the
    # document and `parse_count` on the test side accepts it.
    row1, row2 = surface_counts(ROOT)
    head = re.sub(
        r'(\| distinct `CRATONVM_\*` identifiers appearing anywhere in Rust '
        r'source \| )[0-9,]+( \|)',
        r'\g<1>%s\g<2>' % format(row1, ',d'), head)
    head = re.sub(
        r'(\| exact string literals \(i\.e\. actually named by code, not '
        r'prose\) \| )[0-9,]+( \|)',
        r'\g<1>%s\g<2>' % format(row2, ',d'), head)
    with open(OUT, 'w', encoding='utf-8', newline='\n') as fh:
        fh.write(head + header + '\n'.join(body) + '\n')
    print('wrote %s (%d rows: %d declared, %d allowlisted; '
          'surface %s identifiers / %s literals)'
          % (OUT, len(table), len(declared), len(ALLOWLISTED),
             format(row1, ',d'), format(row2, ',d')))


if __name__ == '__main__':
    main()
