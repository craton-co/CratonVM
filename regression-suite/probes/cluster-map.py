"""Map native registrations into STATE-OWNERSHIP CLUSTERS.

G88-1 N4. Wave 2's unit of work is not the registrar and not the crate -- it is
the set of registrars that between them own one object's state. G88-1 measured
both halves of why:

  §5  retagging a container WITHOUT its view carriers breaks, because a shim
      reading side state and real bytecode writing real fields cannot both be
      half-right. Move the cluster or move nothing.
  §6  cluster boundaries do NOT respect crate boundaries. `java.util.Properties`
      + `Hashtable` is 67 registrations in `native-builtins` against 49 in
      `native-collections`.

This turns a dump into that map. For each class: which registrars register
against it, in which crates, how many registrations, how many invocations.
Classes that share a registrar are transitively one cluster.

Usage:
    cratonvm --java-home <JDK> --dump-native-registry reg.json -cp <cp> <Class>
    python regression-suite/probes/cluster-map.py reg.json [--by=fn|file] [--root DIR]

Read the output as a work list: one cluster is one agent's PR.

STATUS 2026-08-20 (lane H0): the FUNCTION join this file's own limitation
section asked for is now implemented and is the DEFAULT. `--by=file` keeps the
old behaviour for comparison.

  The old file join over-merged: `native-builtins/src/lib.rs` holds dozens of
  unrelated registrars, so every class registered anywhere in it landed in one
  blob -- measured 2026-08-19, a top "cluster" of 9324 registrations over 889
  classes spanning four crates, which is an artefact of the granularity and not
  an ownership group. **Run both and diff them.** Where `fn` splits a `file`
  cluster into many, the file answer was the artefact. Where it does NOT split
  one, the merge is real and the classes genuinely share a registrar.

  How the function join works: `registered_by` is `<file>:<line>`; each source
  file is parsed once for its top-level `fn <name>` definitions with their line
  numbers, and a registration's line is attributed to the last `fn` at or above
  it. That is the same line->enclosing-`fn` technique G79-1 and G88-1 used by
  hand for `native-collections` and `native-awt`.

  What the function join CANNOT see, stated rather than left to be discovered:

    * a registrar that delegates to helpers in the same file still attributes
      to the helper, not the entry point, when the helper itself calls
      `register()`. That SPLITS a real cluster. The `--merge-callers` pass
      partially repairs it by unioning a `fn` with any `fn` in the same file
      that names it in a call, which is a text scan, not a call graph;
    * ambient category windows (`set_category` ... `set_category(prev)`) are a
      SECOND grouping that does not have to coincide with either join. A
      cluster this script calls clean can still be split by a category window,
      which is what made the AWT per-group split tractable and the collections
      one not;
    * a cluster is a claim about STATE, and this script only measures
      REGISTRATION. Two registrars over one class are strong evidence; one
      registrar over two classes that share a side table is invisible here.
      G88-1 §5's map/set cluster was found by breaking it, not by this map.

CONTRACT NOTE, 2026-08-20: contract §8's ban on editing
`native-builtins/src/lib.rs` was LIFTED for wave 2. The gate column below no
longer refuses those clusters; it reports the crate span so the reader can size
the PR. A cluster spanning two crates is still harder than one confined to a
crate -- that was always the real content of the §8 annotation.
"""
import json
import os
import re
import sys
import collections

BS = chr(92)
FN_RE = re.compile(r"^(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+\"[^\"]*\"\s+)?fn\s+([A-Za-z0-9_]+)")


def norm(p):
    return (p or "?").replace(BS, "/")


def crate_of(registered_by):
    return norm(registered_by).split("/src/")[0]


def split_site(registered_by):
    """`<file>:<line>` -> (file, line). Tolerates drive letters and no line."""
    s = norm(registered_by)
    head, sep, tail = s.rpartition(":")
    if sep and tail.isdigit():
        return head, int(tail)
    return s, 0


def resolve(path, root):
    """Map a build-time path to a file under `root`, or return None."""
    if os.path.isfile(path):
        return path
    # Keep the crate-relative tail: everything from the last crate-ish segment
    # that is followed by `/src/`.
    idx = path.rfind("/src/")
    if idx < 0:
        return None
    crate_start = path.rfind("/", 0, idx)
    rel = path[crate_start + 1:] if crate_start >= 0 else path
    cand = os.path.join(root, rel)
    return cand if os.path.isfile(cand) else None


class FnIndex:
    """line -> enclosing top-level `fn`, per source file."""

    def __init__(self, root):
        self.root = root
        self.cache = {}
        self.misses = set()

    def _load(self, path):
        real = resolve(path, self.root)
        if real is None:
            self.misses.add(path)
            self.cache[path] = None
            return None
        fns = []
        callers = collections.defaultdict(set)
        try:
            with open(real, encoding="utf-8", errors="replace") as fh:
                lines = fh.readlines()
        except OSError:
            self.misses.add(path)
            self.cache[path] = None
            return None
        for n, line in enumerate(lines, 1):
            m = FN_RE.match(line.lstrip())
            if m and (len(line) - len(line.lstrip())) <= 4:
                fns.append((n, m.group(1)))
        # Text-scan call edges so `--merge-callers` can union a helper with the
        # entry point that names it. Not a call graph; see the module docstring.
        # One alternation, not a name-by-name scan: `native-collections/src/
        # lib.rs` is ~70k lines with hundreds of `fn`s, and the naive nested
        # loop is minutes of pure substring searching for no extra recall.
        names = sorted({name for _, name in fns}, key=len, reverse=True)
        if names:
            call_re = re.compile(r"(?<![A-Za-z0-9_])(" + "|".join(re.escape(n) for n in names) + r")\s*\(")
            for n, line in enumerate(lines, 1):
                found = call_re.findall(line)
                if not found:
                    continue
                owner = _owner(fns, n)
                if not owner:
                    continue
                for name in found:
                    if name != owner:
                        callers[name].add(owner)
        self.cache[path] = (fns, callers)
        return self.cache[path]

    def owner(self, path, line):
        entry = self.cache.get(path, "unset")
        if entry == "unset":
            entry = self._load(path)
        if not entry:
            return None
        return _owner(entry[0], line)

    def callers(self, path):
        entry = self.cache.get(path, "unset")
        if entry == "unset":
            entry = self._load(path)
        return entry[1] if entry else {}


def _owner(fns, line):
    best = None
    for n, name in fns:
        if n <= line:
            best = name
        else:
            break
    return best


class Union:
    def __init__(self):
        self.parent = {}

    def find(self, x):
        self.parent.setdefault(x, x)
        while self.parent[x] != x:
            self.parent[x] = self.parent[self.parent[x]]
            x = self.parent[x]
        return x

    def union(self, a, b):
        ra, rb = self.find(a), self.find(b)
        if ra != rb:
            self.parent[ra] = rb


def main(argv):
    path = "reg.json"
    mode = "fn"
    root = os.getcwd()
    merge_callers = True
    for a in argv:
        if a.startswith("--by="):
            mode = a.split("=", 1)[1]
        elif a.startswith("--root="):
            root = a.split("=", 1)[1]
        elif a == "--no-merge-callers":
            merge_callers = False
        elif not a.startswith("-"):
            path = a

    d = json.load(open(path, encoding="utf-8"))
    rows = [r for r in d.get("natives", []) if r.get("registered_by")]
    if not rows:
        print("no rows carry `registered_by` -- wrong dump, or a build without it")
        return 1

    index = FnIndex(root)
    cls_crates = collections.defaultdict(set)
    cls_sites = collections.defaultdict(set)
    stats = collections.defaultdict(lambda: [0, 0])
    unresolved = 0

    for r in rows:
        f, line = split_site(r["registered_by"])
        cls_crates[r["class"]].add(crate_of(r["registered_by"]))
        if mode == "file":
            site = f
        else:
            fn = index.owner(f, line)
            if fn is None:
                unresolved += 1
                site = f  # fall back to the file, and SAY SO below
            else:
                site = "%s::%s" % (f, fn)
        cls_sites[r["class"]].add(site)
        st = stats[r["class"]]
        st[0] += 1
        st[1] += r.get("invocations", 0)

    u = Union()
    by_site = collections.defaultdict(list)
    for c, sites in cls_sites.items():
        for s in sites:
            by_site[s].append(c)
    for _, classes in by_site.items():
        for c in classes[1:]:
            u.union(classes[0], c)

    if mode != "file" and merge_callers:
        for site in list(by_site):
            if "::" not in site:
                continue
            f, fn = site.rsplit("::", 1)
            for caller in index.callers(f).get(fn, ()):
                other = "%s::%s" % (f, caller)
                if other in by_site:
                    u.union(by_site[site][0], by_site[other][0])

    clusters = collections.defaultdict(list)
    for c in cls_sites:
        clusters[u.find(c)].append(c)

    out = []
    for _, classes in clusters.items():
        regs = sum(stats[c][0] for c in classes)
        inv = sum(stats[c][1] for c in classes)
        crates = sorted({s for c in classes for s in cls_crates[c]})
        sites = sorted({s for c in classes for s in cls_sites[c]})
        out.append((regs, inv, len(classes), crates, sorted(classes), sites))
    out.sort(reverse=True)

    print("join=%s  clusters=%d  registrations=%d" % (mode, len(out), len(rows)))
    if unresolved:
        print("WARNING: %d of %d registrations could not be attributed to a `fn` "
              "and fell back to their FILE -- those clusters are as coarse as "
              "the old join. Pass --root=<workspace> if the sources are "
              "elsewhere." % (unresolved, len(rows)))
    if index.misses:
        print("         unreadable sources (%d): %s"
              % (len(index.misses), ", ".join(sorted(index.misses)[:3])))
    print()
    print("%-6s %-9s %-7s %-7s %s" % ("REGS", "INVOC", "CLASSES", "SITES", "CRATES"))
    for regs, inv, ncls, crates, classes, sites in out[:30]:
        span = "" if len(crates) == 1 else "   <-- spans %d crates" % len(crates)
        print("%-6d %-9d %-7d %-7d %s%s"
              % (regs, inv, ncls, len(sites), ",".join(crates), span))
        print("        classes: %s%s"
              % (", ".join(classes[:4]), " ..." if len(classes) > 4 else ""))
        print("        sites:   %s%s"
              % ("; ".join(s.split("/src/")[-1] for s in sites[:3]),
                 " ..." if len(sites) > 3 else ""))
    print()
    one = [o for o in out if len(o[3]) == 1]
    print("clusters confined to ONE crate: %d of %d, holding %d registrations"
          % (len(one), len(out), sum(o[0] for o in one)))
    # A zero-invocation cluster is not a clean one -- G33-1: `invocations == 0`
    # proves nothing, because no vector may have exercised the surface.
    quiet = [o for o in out if o[1] == 0]
    print("clusters with ZERO invocations in this dump: %d of %d, holding %d "
          "registrations -- NOT evidence they are dead (G33-1); it is evidence "
          "this vector did not ask." % (len(quiet), len(out), sum(o[0] for o in quiet)))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
