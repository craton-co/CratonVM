#!/usr/bin/env python3
"""Per JIT round-21 commit: how much of its added .rs code is missing on dev.

Merge 624fae0b49 resolved round 21 (its second parent) to dev's side. For each
non-merge commit in M^1..M^2 touching *.rs, take its distinctive added lines
(stripped, >= 25 chars, not `//` comments) and report, comparing with all
whitespace removed (a `cargo fmt` reflow is not a loss):
  * dropped    = lines present at the round-21 tip (M^2) but absent on DEV;
  * at_r21_tip / on_dev = share of the commit's lines present there;
  * fwd / rev  = whether `git apply --check` (forward / reverse) of the commit
    succeeds on a DEV worktree (WT).
Prints a TSV sorted by dropped. Page:
docs/internal/jit/jr21-merge-624fae0b49-dropped-round-21-code-20261026.md
"""
import subprocess, sys, os, re

REPO = '/root/CVM'
WT = '/root/wt-aud'
M = '624fae0b49'
DEV = 'aud-dev'


def git(*a, cwd=REPO, check=True):
    r = subprocess.run(['git', *a], cwd=cwd, capture_output=True, text=True, errors='replace')
    if check and r.returncode != 0:
        raise RuntimeError(r.stderr)
    return r.stdout


blob_cache = {}


def blob(rev, path):
    k = (rev, path)
    if k not in blob_cache:
        r = subprocess.run(['git', 'show', f'{rev}:{path}'], cwd=REPO, capture_output=True, text=True, errors='replace')
        blob_cache[k] = r.stdout if r.returncode == 0 else None
    return blob_cache[k]


def added_lines(c):
    out = git('show', '--format=', '--unified=0', c, '--', '*.rs')
    per = {}
    cur = None
    for ln in out.splitlines():
        if ln.startswith('+++ '):
            cur = ln[6:] if ln.startswith('+++ b/') else None
            continue
        if cur and ln.startswith('+') and not ln.startswith('+++'):
            s = ln[1:].strip()
            if len(s) < 25 or s.startswith('//'):
                continue
            per.setdefault(cur, []).append(s)
    return per


WS = re.compile(r'\s+')
norm_cache = {}


def norm(rev, f):
    k = (rev, f)
    if k not in norm_cache:
        norm_cache[k] = WS.sub('', blob(rev, f) or '').replace(',)', ')').replace(',}', '}').replace(',]', ']')
    return norm_cache[k]


def present(per, rev):
    out = set()
    for f, lines in per.items():
        text = norm(rev, f)
        for i, s in enumerate(lines):
            t = WS.sub('', s).rstrip(',')
            if t in text:
                out.add((f, i))
    return out


def main():
    commits = git('rev-list', '--no-merges', '--reverse', f'{M}^1..{M}^2', '--', '*.rs').split()
    rows = []
    for c in commits:
        per = added_lines(c)
        tot = sum(len(v) for v in per.values())
        tip = present(per, M + '^2')
        dev = present(per, DEV)
        dropped = len(tip - dev)
        sv = len(dev) / tot if tot else 1.0
        am = len(tip) / tot if tot else 1.0
        patch = git('show', '--format=', c, '--', '*.rs')
        pf = f'/tmp/aud-{c}.diff'
        open(pf, 'w').write(patch)
        fwd = subprocess.run(['git', 'apply', '--check', pf], cwd=WT, capture_output=True).returncode == 0
        rev = subprocess.run(['git', 'apply', '--check', '-R', pf], cwd=WT, capture_output=True).returncode == 0
        os.remove(pf)
        subj = git('log', '-1', '--format=%s', c).strip()
        files = ','.join(sorted(per))[:120]
        rows.append((-dropped, c[:10], tot, am, sv, fwd, rev, subj[:140], files))
    rows.sort()
    print('dropped\tcommit\tlines\tat_r21_tip\ton_dev\tfwd_applies\trev_applies\tsubject\tfiles')
    for d, c, tot, am, sv, fwd, rev, subj, files in rows:
        print(f'{-d}\t{c}\t{tot}\t{am:.2f}\t{sv:.2f}\t{int(fwd)}\t{int(rev)}\t{subj}\t{files}')


main()
