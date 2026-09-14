"""Classify the `--jdk-only` native-shadows-bytecode population.

H14. The strict arm prints ONE number for the whole defect:

    native-shadows-bytecode, UNION: 1402 native-won, 477 bytecode-won

and until this script existed nobody had ever bucketed those rows. Every plan
in `docs/known-issues/jdk-only/` was argued from hand-picked examples, and the
number itself was a FLOOR of unknown depth until 2026-08-20 (`H1-1`: the
observation sink capped at 256 rows and announced saturation through a boolean
nothing read).

WHAT IT JOINS, AND WHY THE JOIN IS THE HARD PART
------------------------------------------------
A `native-shadows-bytecode` row (`types::error::JdkOnlyViolation::to_json`)
carries `class`, `method`, `descriptor`, `native_kind` and `outcome` -- and
NOT `registered_by`. So "which registrar owns this defect" is not a field, it
is a join:

    report row  (class, method, descriptor)
        -> `--dump-native-registry` entry with the same triple
        -> its `registered_by` == "<file>:<line>"
        -> the last top-level `fn` at or above <line> in that file

The last hop is `cluster-map.py`'s REGISTRAR-FUNCTION join and is not
optional. A file-level join conflates unrelated registrars: `H0-1` measured a
file "cluster" of 9324 registrations over 889 classes spanning four crates,
which is an artefact of granularity, and `G88-1` §6 published a plan claiming a
family was "58% inside native-builtins/src/lib.rs" when that file holds ZERO
registrations for it. `--by=file` is kept only so the two can be diffed.

`registered_by` is REDACTED unless the dump was taken with `--explain-jdk-only`
(`vm-cli/src/main.rs`: `let verbose = args.explain_jdk_only`). A dump without
it produces a large `no-registrar` bucket and no error, so this script REFUSES
a registry whose sites are redacted rather than under-attributing quietly.

WHAT IT WILL NOT TELL YOU
-------------------------
* A registrar is not a state-ownership cluster. `cluster-map.py`'s docstring
  is the authority here: two registrars over one class is strong evidence, one
  registrar over two classes that share a side table is invisible to both
  scripts. The `--clusters` output unions registrars that share a class, which
  is the same transitive rule `cluster-map.py` uses and the same limitation.
* A row that no registry entry matches is reported in `unattributed`, never
  dropped. Silent truncation is the exact defect that made `943` look like a
  total for months.
* The union is over the vectors that RAN. 104 vectors is not the world -- the
  corpus has no AWT vector at all (`G79-1`).

USAGE
-----
    python regression-suite/probes/shadow-triage.py \
        --reports <DIR of per-vector *.json> \
        --registry <reg.json from --dump-native-registry --explain-jdk-only> \
        [--root .] [--by fn|file] [--outcome native-won|bytecode-won|both]
        [--top 25] [--csv rows.csv] [--clusters]

The per-vector reports are written by `regression-suite/run.sh` into a
PID-scoped directory that it DELETES at the end of the run. Capture them while
the run is live (poll-and-copy) or pass `--jdk-only-report` yourself per
vector; this script takes a directory either way.
"""
import argparse
import collections
import csv
import json
import os
import re
import sys

BS = chr(92)

# Same recogniser as cluster-map.py, deliberately: two scripts that disagree
# about what a top-level `fn` is would attribute the same line to two owners.
FN_RE = re.compile(
    r"^(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?"
    r"(?:extern\s+\"[^\"]*\"\s+)?fn\s+([A-Za-z0-9_]+)"
)

SHADOW_KIND = '"kind":"native-shadows-bytecode"'


def norm(p):
    return (p or "?").replace(BS, "/")


def crate_of(site):
    return norm(site).split("/src/")[0]


def split_site(site):
    """`<file>:<line>` -> (file, line). Tolerates drive letters and no line."""
    s = norm(site)
    head, sep, tail = s.rpartition(":")
    if sep and tail.isdigit():
        return head, int(tail)
    return s, 0


def resolve(path, root):
    """Map a build-time path to a file under `root`, or None."""
    if os.path.isfile(path):
        return path
    idx = path.rfind("/src/")
    if idx < 0:
        return None
    crate_start = path.rfind("/", 0, idx)
    rel = path[crate_start + 1:] if crate_start >= 0 else path
    cand = os.path.join(root, rel)
    return cand if os.path.isfile(cand) else None


class FnIndex:
    """line -> enclosing `fn`, per source file.

    `max_indent` is the discriminator, and it is NOT cosmetic. `cluster-map.py`
    admits any `fn` indented four columns or fewer, which also admits a
    **nested** `fn` declared inside another function body -- Rust allows that
    and this tree uses it. Every registration after such a helper is then
    attributed to the helper instead of to the registrar that encloses both.

    MEASURED, H14: with `max_indent=4` the largest bucket in the whole shadow
    population is `native-builtins/src/lib.rs::url_path_or_file_field` at 74
    rows, and that is a nested `fn` at line 13348 indented four columns. With
    `max_indent=0` those rows join their real registrar. Run both and diff:
    a bucket that MOVES was an artefact of the granularity.
    """

    def __init__(self, root, max_indent=0):
        self.root = root
        self.max_indent = max_indent
        self.cache = {}
        self.misses = set()

    def _load(self, path):
        real = resolve(path, self.root)
        if real is None:
            self.misses.add(path)
            self.cache[path] = None
            return None
        try:
            with open(real, encoding="utf-8", errors="replace") as fh:
                lines = fh.readlines()
        except OSError:
            self.misses.add(path)
            self.cache[path] = None
            return None
        fns = []
        for n, line in enumerate(lines, 1):
            stripped = line.lstrip()
            m = FN_RE.match(stripped)
            if m and (len(line) - len(stripped)) <= self.max_indent:
                fns.append((n, m.group(1)))
        self.cache[path] = fns
        return fns

    def owner(self, path, line):
        entry = self.cache.get(path, "unset")
        if entry == "unset":
            entry = self._load(path)
        if not entry:
            return None
        best = None
        for n, name in entry:
            if n <= line:
                best = name
            else:
                break
        return best


def java_package(cls):
    cls = cls.replace(".", "/")
    if "/" not in cls:
        return "<default>"
    return cls.rsplit("/", 1)[0]


def load_reports(dirpath):
    """(rows, per_vector) -- rows keyed by the four identifying fields."""
    rows = {}
    per_vector = collections.defaultdict(set)
    files = sorted(
        f for f in os.listdir(dirpath) if f.endswith(".json")
    )
    if not files:
        sys.exit("shadow-triage: no *.json under %s" % dirpath)
    for fname in files:
        vector = fname[:-5]
        with open(os.path.join(dirpath, fname), encoding="utf-8",
                  errors="replace") as fh:
            for line in fh:
                if SHADOW_KIND not in line:
                    continue
                text = line.strip()
                if text.endswith(","):
                    text = text[:-1]
                try:
                    row = json.loads(text)
                except ValueError:
                    continue
                key = (
                    row.get("class", "?"),
                    row.get("method", "?"),
                    row.get("descriptor", "?"),
                    row.get("outcome", "?"),
                )
                rows[key] = row
                per_vector[key].add(vector)
    return rows, per_vector, files


def load_registry(path):
    with open(path, encoding="utf-8", errors="replace") as fh:
        doc = json.load(fh)
    by_triple = collections.defaultdict(list)
    for e in doc.get("natives", []):
        by_triple[(e.get("class"), e.get("name"), e.get("descriptor"))].append(e)
    return doc, by_triple


def pick(entries):
    """The registration a dispatch would REACH: the slot owner, if any."""
    if not entries:
        return None
    owners = [e for e in entries if e.get("owns_slot")]
    return owners[0] if owners else entries[0]


def pct(n, d):
    return "0.0" if not d else "%.1f" % (100.0 * n / d)


def main(argv):
    ap = argparse.ArgumentParser(add_help=True)
    ap.add_argument("--reports", required=True)
    ap.add_argument("--registry", required=True)
    ap.add_argument("--root", default=os.getcwd())
    ap.add_argument("--by", default="fn", choices=("fn", "file"))
    ap.add_argument("--outcome", default="native-won",
                    choices=("native-won", "bytecode-won", "both"))
    ap.add_argument("--top", type=int, default=25)
    ap.add_argument("--csv", default=None)
    ap.add_argument("--clusters", action="store_true")
    ap.add_argument("--fn-indent", type=int, default=0,
                    help="max indent for a `fn` to count as a registrar. "
                         "0 = true top level (default). 4 = cluster-map.py's "
                         "rule, which also admits NESTED fns; run both, diff.")
    ap.add_argument("--allow-redacted", action="store_true",
                    help="proceed even if registered_by looks redacted")
    args = ap.parse_args(argv)

    rows, per_vector, files = load_reports(args.reports)
    doc, by_triple = load_registry(args.registry)

    sites = [e.get("registered_by") for e in doc.get("natives", [])]
    known = [s for s in sites if s]
    redacted = [s for s in known if "/src/" not in norm(s)]
    if known and len(redacted) > len(known) // 2 and not args.allow_redacted:
        sys.exit(
            "shadow-triage: %d of %d registered_by sites carry no /src/ path -- "
            "the dump was taken WITHOUT --explain-jdk-only, so the sites are "
            "redacted and every registrar bucket would be wrong. Re-dump with "
            "--explain-jdk-only, or pass --allow-redacted to override."
            % (len(redacted), len(known))
        )

    wanted = [k for k in rows
              if args.outcome == "both" or k[3] == args.outcome]
    total = len(wanted)

    fnidx = FnIndex(args.root, args.fn_indent)
    recs = []
    unattributed = []
    for key in wanted:
        cls, meth, desc, outcome = key
        entries = by_triple.get((cls, meth, desc), [])
        entry = pick(entries)
        site = entry.get("registered_by") if entry else None
        if site:
            fpath, line = split_site(site)
            fn = fnidx.owner(fpath, line) if line else None
            crate = crate_of(site)
        else:
            fpath, line, fn, crate = None, 0, None, None
        # CRATE-qualified on purpose: four crates register here and three of
        # them have a `src/lib.rs`, so a bare `lib.rs::register_io_natives`
        # merges native-io's registrar with any same-named one elsewhere. A
        # bucket key that can collide is the file-join artefact in miniature.
        bucket = (
            "%s/%s::%s" % (crate, os.path.basename(fpath), fn) if (fpath and fn)
            else (norm(fpath) if fpath else "<no-registrar>")
        ) if args.by == "fn" else (norm(fpath) if fpath else "<no-registrar>")
        rec = {
            "class": cls,
            "method": meth,
            "descriptor": desc,
            "outcome": outcome,
            "native_kind": rows[key].get("native_kind"),
            "package": java_package(cls),
            "registrar_fn": fn,
            "registrar_file": norm(fpath) if fpath else None,
            "registrar_line": line,
            "crate": crate,
            "bucket": bucket,
            "registry_entries": len(entries),
            "owns_slot": bool(entry.get("owns_slot")) if entry else None,
            "kind_stated": entry.get("kind_stated") if entry else None,
            "real_declared": (entry.get("real_declaring_method") or {}).get("declared")
            if entry else None,
            "real_has_code": (entry.get("real_declaring_method") or {}).get("has_code")
            if entry else None,
            # The IMAGE columns, not the RUN columns, are the ones that decide
            # whether a shadow is retirable: `real_declaring_method` says only
            # whether THIS workload loaded the class, so `declared: false` there
            # is the absence of a verdict (`G33-1`). `image_declaring_method`
            # parses the bytes on the class path and answers for every row.
            "image_declared": (entry.get("image_declaring_method") or {}).get("declared")
            if entry else None,
            "image_has_code": (entry.get("image_declaring_method") or {}).get("has_code")
            if entry else None,
            "image_acc_native": (entry.get("image_declaring_method") or {}).get("acc_native")
            if entry else None,
            "image_inherited_from": (entry.get("image_declaring_method") or {}).get("inherited_from")
            if entry else None,
            "vectors": len(per_vector[key]),
        }
        recs.append(rec)
        if not site:
            unattributed.append(rec)

    print("== shadow-triage ==")
    print("reports: %d vector files from %s" % (len(files), args.reports))
    print("registry: %d entries, mode=%s, schema=%s"
          % (len(doc.get("natives", [])), doc.get("mode"),
             doc.get("schema_version")))
    nw = sum(1 for k in rows if k[3] == "native-won")
    bw = sum(1 for k in rows if k[3] == "bytecode-won")
    print("rows in reports: %d distinct  (native-won %d, bytecode-won %d)"
          % (len(rows), nw, bw))
    print("selected outcome=%s: %d rows" % (args.outcome, total))
    att = total - len(unattributed)
    print("attributed to a registration site: %d / %d (%s%%);  UNATTRIBUTED %d"
          % (att, total, pct(att, total), len(unattributed)))
    if fnidx.misses:
        print("source files not found under --root (%d): %s"
              % (len(fnidx.misses), ", ".join(sorted(fnidx.misses)[:5])))
    nofn = sum(1 for r in recs if r["registrar_file"] and not r["registrar_fn"])
    if nofn:
        print("site resolved but no enclosing top-level fn: %d" % nofn)

    def table(title, keyfn, top):
        counts = collections.Counter(keyfn(r) for r in recs)
        print("")
        print("-- %s (%d distinct) --" % (title, len(counts)))
        run = 0
        for i, (k, n) in enumerate(counts.most_common(top), 1):
            run += n
            print("%3d. %6d  %5s%%  cum %5s%%  %s"
                  % (i, n, pct(n, total), pct(run, total), k))
        for cut in (10, 25, 50, 100):
            if cut <= len(counts):
                s = sum(n for _, n in counts.most_common(cut))
                print("    top %-3d = %5d / %d  (%s%%)"
                      % (cut, s, total, pct(s, total)))
        return counts

    reg_counts = table("by REGISTRAR (%s join)" % args.by,
                       lambda r: r["bucket"], args.top)
    table("by CRATE", lambda r: r["crate"] or "<none>", args.top)
    table("by JAVA PACKAGE", lambda r: r["package"], args.top)
    table("by JAVA CLASS", lambda r: r["class"], args.top)
    table("by NativeKind tag (report)", lambda r: r["native_kind"], 10)
    table("by registry kind_stated", lambda r: str(r["kind_stated"]), 10)
    table("by IMAGE verdict (declared/has_code/acc_native)",
          lambda r: "declared=%s has_code=%s acc_native=%s inherited=%s"
                    % (r["image_declared"], r["image_has_code"],
                       r["image_acc_native"],
                       "yes" if r["image_inherited_from"] else "no"), 10)
    table("by vectors that observed the row",
          lambda r: "%d vector(s)" % r["vectors"], 12)

    if args.clusters:
        # Transitive: registrars that share a Java class are one cluster. Same
        # rule as cluster-map.py, same limitation -- it measures REGISTRATION,
        # not state.
        parent = {}

        def find(x):
            parent.setdefault(x, x)
            while parent[x] != x:
                parent[x] = parent[parent[x]]
                x = parent[x]
            return x

        def union(a, b):
            ra, rb = find(a), find(b)
            if ra != rb:
                parent[ra] = rb

        by_class = collections.defaultdict(set)
        for r in recs:
            by_class[r["class"]].add(r["bucket"])
        for bs in by_class.values():
            bs = sorted(bs)
            for b in bs[1:]:
                union(bs[0], b)
        cl_rows = collections.Counter()
        cl_regs = collections.defaultdict(set)
        cl_cls = collections.defaultdict(set)
        cl_crates = collections.defaultdict(set)
        for r in recs:
            root = find(r["bucket"])
            cl_rows[root] += 1
            cl_regs[root].add(r["bucket"])
            cl_cls[root].add(r["class"])
            cl_crates[root].add(r["crate"] or "<none>")
        print("")
        print("-- OWNERSHIP CLUSTERS (registrars transitively sharing a class):"
              " %d --" % len(cl_rows))
        run = 0
        for i, (root, n) in enumerate(cl_rows.most_common(args.top), 1):
            run += n
            print("%3d. %6d rows  cum %5s%%  regs=%-4d classes=%-4d crates=%s"
                  % (i, n, pct(run, total), len(cl_regs[root]),
                     len(cl_cls[root]), ",".join(sorted(cl_crates[root]))))
            print("      lead: %s" % root)
            print("      classes: %s%s"
                  % (", ".join(sorted(cl_cls[root])[:6]),
                     " ..." if len(cl_cls[root]) > 6 else ""))

    if unattributed:
        print("")
        print("-- UNATTRIBUTED (%d): reported, never dropped --"
              % len(unattributed))
        by_cls = collections.Counter(r["class"] for r in unattributed)
        for k, n in by_cls.most_common(args.top):
            print("    %5d  %s" % (n, k))

    if args.csv:
        cols = ["class", "method", "descriptor", "outcome", "native_kind",
                "package", "registrar_fn", "registrar_file", "registrar_line",
                "crate", "bucket", "registry_entries", "owns_slot",
                "kind_stated", "real_declared", "real_has_code",
                "image_declared", "image_has_code", "image_acc_native",
                "image_inherited_from", "vectors"]
        with open(args.csv, "w", newline="", encoding="utf-8") as fh:
            w = csv.DictWriter(fh, fieldnames=cols)
            w.writeheader()
            for r in recs:
                w.writerow(r)
        print("")
        print("wrote %d rows to %s" % (len(recs), args.csv))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
