"""Map native registrations into STATE-OWNERSHIP CLUSTERS.

G88-1 N4. Wave 2's unit of work is not the registrar and not the crate — it is
the set of registrars that between them own one object's state. G88-1 measured
both halves of why:

  §5  retagging a container WITHOUT its view carriers breaks, because a shim
      reading side state and real bytecode writing real fields cannot both be
      half-right. Move the cluster or move nothing.
  §6  cluster boundaries do NOT respect crate boundaries. `java.util.Properties`
      + `Hashtable` is 67 registrations in `native-builtins` against 49 in
      `native-collections`, and contract §8 forbids editing the former this
      wave — so that cluster is not wave-1 work no matter how it is approached.

This turns a dump into that map. For each class: which registrars register
against it, in which crates, how many registrations, how many invocations.
Classes that share a registrar are transitively one cluster.

Usage:
    cratonvm --java-home <JDK> --dump-native-registry reg.json -cp <cp> <Class>
    python regression-suite/probes/cluster-map.py reg.json

Read the output as a work list: a cluster confined to ONE crate is a candidate
wave-1 PR; a cluster spanning `native-builtins` is not, by §8.

KNOWN LIMITATION — the join is per FILE, and that over-merges.
`native-builtins/src/lib.rs` holds dozens of unrelated registrars in one file,
so every class registered anywhere in it lands in a single blob: measured
2026-08-19, the top "cluster" is 9324 registrations over 889 classes spanning
four crates, which is an artefact of the granularity and not a real ownership
group. Only the SMALL clusters here are trustworthy as-is.

To sharpen it, join by REGISTRAR FUNCTION instead: map each `registered_by`
line number to its enclosing `fn register_*` (the technique used in G79-1 and
G88-1 for `native-collections`), then union classes that share a FUNCTION. That
needs per-crate source parsing, which is why it is not done here — the file
join was the cheap version, and its failure mode is stated rather than left to
be discovered.

What survives the limitation and is worth acting on:
  * `native-awt` — 199 registrations, 37 classes, ONE crate. That it is a clean
    single-crate cluster is why the per-group split landed there (G80-1 §4b);
  * only 8 of 48 clusters are confined to a non-`native-builtins` crate, and
    they hold 303 registrations between them. The rest touch the file §8
    protects. That ratio is the honest scale of what wave 1 can reach.
"""
import json
import sys
import collections

BS = chr(92)


def crate_of(registered_by):
    return (registered_by or "?").replace(BS, "/").split("/src/")[0]


def main(path):
    d = json.load(open(path, encoding="utf-8"))
    rows = [r for r in d.get("natives", []) if r.get("registered_by")]

    # class -> registrar-sites, and registrar-site -> classes
    cls_sites = collections.defaultdict(set)
    site_cls = collections.defaultdict(set)
    stats = collections.defaultdict(lambda: [0, 0])  # class -> [regs, invocations]
    for r in rows:
        site = crate_of(r["registered_by"])
        cls_sites[r["class"]].add(site)
        site_cls[site].add(r["class"])
        st = stats[r["class"]]
        st[0] += 1
        st[1] += r.get("invocations", 0)

    # Union-find over classes joined by a shared crate-site is too coarse; join
    # by shared registered_by FILE instead, which is the registrar granularity
    # the retag actually operates on.
    file_of = lambda rb: (rb or "?").replace(BS, "/").rsplit(":", 1)[0]
    cls_files = collections.defaultdict(set)
    for r in rows:
        cls_files[r["class"]].add(file_of(r["registered_by"]))

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

    by_file = collections.defaultdict(list)
    for c, files in cls_files.items():
        for f in files:
            by_file[f].append(c)
    for f, classes in by_file.items():
        for c in classes[1:]:
            union(classes[0], c)

    clusters = collections.defaultdict(list)
    for c in cls_files:
        clusters[find(c)].append(c)

    out = []
    for _, classes in clusters.items():
        regs = sum(stats[c][0] for c in classes)
        inv = sum(stats[c][1] for c in classes)
        crates = sorted({s for c in classes for s in cls_sites[c]})
        out.append((regs, inv, len(classes), crates, sorted(classes)))
    out.sort(reverse=True)

    print("%-6s %-9s %-6s %s" % ("REGS", "INVOC", "CLASSES", "CRATES"))
    for regs, inv, ncls, crates, classes in out[:25]:
        gate = "" if len(crates) == 1 and "native-builtins" not in crates else "   <-- SPANS/TOUCHES native-builtins: NOT wave-1 (contract §8)"
        print("%-6d %-9d %-6d %s%s" % (regs, inv, ncls, ",".join(crates), gate))
        print("        e.g. %s" % ", ".join(classes[:4]))
    print()
    single = [o for o in out if len(o[3]) == 1 and "native-builtins" not in o[3]]
    print("clusters confined to ONE crate that is not native-builtins: %d of %d"
          % (len(single), len(out)))
    print("their total registrations: %d" % sum(o[0] for o in single))


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "reg.json")
