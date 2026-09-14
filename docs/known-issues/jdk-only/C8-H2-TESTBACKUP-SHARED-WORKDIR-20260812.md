# C8 — `TestBackup` (H2) is a known-flaky fixture: it fails in BOTH arms from a shared working directory

**Date:** 2026-08-12 **Lane:** C8 **Status:** NOT A VM DEFECT — a fixture /
harness hazard. Standing warning.

---

## 1. The finding

`org.h2.test.db.TestBackup` fails intermittently with

```
org.h2.mvstore.MVStoreException: Chunk 2 not found
```

**in both arms, independent of any VM setting**, when it is run from a working
directory that already contains H2 database files from an earlier run. Run
alone in a fresh working directory it is green everywhere —
`P4A-H2-DIVERGENCES-20260812.md` §3c measured exactly that:

```
TestBackup --jdk-only  rc=0  CORPUS-END ... completed=true
TestBackup --real-jdk  rc=0  CORPUS-END ... completed=true
TestBackup HotSpot     rc=0  CORPUS-END ... completed=true
```

It had already been reported once as a `DIVERGE` against the VM and retracted.

**Provenance of this record:** the flake was handed to this lane as a measured
finding; this lane did not reproduce it, because it may not run the VM this
wave. What this lane did was (a) confirm the mechanism is live in the driver,
(b) build the mechanism that fixes it, and (c) write the warning down so the
next one-shot run does not spend a lane re-deriving it.

---

## 2. Why it happens, mechanically

`corpus_workdir` for the `h2` corpus returns the corpus root, and **both arms
run there**, one after the other, CratonVM first:

1. the CratonVM arm runs and leaves `data/` behind;
2. the HotSpot arm starts in the same directory and inherits it;
3. `TestBackup` copies/restores a store that is not the one it created.

So the second arm is not running the same workload as the first, and a corrupt
or half-written store from run *N* is an input to run *N+1*. Two arms compared
under different inputs are not a comparison. This is the same family as the
`$wd`-never-used defect: the failure appears on whichever side happens to
inherit the mess, including the **reference** side, where it reads as a VM
defect and gets scored as one.

`MVStoreException: Chunk N not found` is a known H2-side shape on this host in
other contexts too — see `internal/fixed-suite-bugs/h2-suite-bugs/`
(`TestLob`: `Chunk 18 not found`, `Chunk 6 not found`, both adjudicated as not
CratonVM defects). A `Chunk N not found` in a shared-cwd run is *presumed
fixture* until a fresh-cwd run says otherwise.

---

## 3. The remedy

`run-corpus.sh` now supports `CORPUS_CLEAN_PATHS`: a space-separated list of
plain relative paths, removed **before each arm**, refusing absolute paths,
`..` and globs. Exercised: a planted `data/store.mv.db` is removed; `../escape`
and `/etc` are refused with a log line.

**No corpus declares it yet.** `regression-suite/corpus/corpora.d/h2.sh` needs

```sh
CORPUS_CLEAN_PATHS="data"
```

which is NOMINATION 1 of `C8-CORPUS-HARNESS-DEFECTS-20260812.md` (with the exact
literal replacement text). Until that lands, **every H2 corpus run inherits the
previous run's databases**, and the driver header will say so:
`# workdir=… clean_before_each_arm='<none>'`.

`apps/h2database-suite-runner/run-h2-suite.sh` already solved this its own way —
`run_one_class` gives every class its own `$outdir/workdirs/<class>` — which is
the precedent for the corpus driver doing something equivalent.

---

## 4. Rules for anyone running H2 workloads

1. **Never adjudicate a one-shot H2 row from a shared working directory.**
   `rm -rf data` before each arm, or declare `CORPUS_CLEAN_PATHS`.
2. **`Chunk N not found` is not a VM finding until it survives a fresh cwd** in
   the arm it appeared in, with the other arm still green.
3. If a `TestBackup` row is red, check `# workdir=` and
   `clean_before_each_arm=` in the run's TSV header **before** reading the
   verdict column.
4. Watch for `data/` appearing as untracked files in the git worktree: that is
   the same defect writing databases into the repository, and it means the run
   was not in the corpus root at all.

---

## 5. What is still unknown

Whether `TestBackup` is *also* flaky in a fresh cwd under concurrency — the
three green runs in §3c were sequential and alone. This record claims only the
shared-directory mechanism. If a fresh-cwd, cleaned run ever produces
`Chunk N not found` again, that is a new finding and needs its own record; do
not fold it into this one.
