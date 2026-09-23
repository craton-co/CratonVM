# H2 — `FileSystem.needToResolveAgainstDefaultDirectory()` `NoSuchMethodError` cluster — investigated, NOT fixed (OPEN 2026-09-22)

| | |
|---|---|
| **Status** | OPEN. Root-cause mechanism identified with strong supporting evidence; the defect could not be reproduced on demand this session, so no fix was implemented or landed. |
| **Scope** | `org.h2.test.db.TestAlterSchemaRename`, `TestTriggersConstraints`, `TestView`, `TestSampleApps` (in-process `javac`, H2's `SourceCompiler`), and now also confirmed `org.h2.test.db.TestCases` (same exact exception, see below) — 5 of the 22 "genuinely new" classes from `nonpassed-classbyclass-census-20260922.md`. |
| **Symptom** | `NoSuchMethodError: 'boolean java.nio.file.FileSystem.needToResolveAgainstDefaultDirectory()'`, thrown from `UnixPath.getByteArrayForSysCalls` during javac's `Locations$SystemModulesLocationHandler.initSystemModules`. Measured in the buffer-race fix's own regression rerun (`postfix` tag, 2026-09-22 16:50-18:57): `TestAlterSchemaRename` FAIL 902ms, `TestTriggersConstraints` FAIL 1763ms, `TestView` FAIL 1093ms, `TestSampleApps` FAIL 3253ms, `TestCases` FAIL 2101ms — all five with the identical `NoSuchMethodError` signature. |

## The mechanism (high confidence, not directly witnessed live this session)

`native-builtins/src/phases_late/nio_file.rs::p57_default_filesystem_singleton`
(line ~16750) caches the default `FileSystem` object in the REAL
`FileSystems$DefaultFileSystemHolder.defaultFileSystem` static field, exactly
once, on the **very first call**, and returns that same cached object
**forever after** — no retry, no re-evaluation:

```rust
if let Some((cid, idx)) = slot {
    if let Value::Object(Some(fs)) = ctx.get_static_field(cid, idx) {
        return Ok(fs);                       // cache hit — always wins from here on
    }
    let fs = p57_alloc_default_filesystem(ctx)?;
    ctx.set_static_field(cid, idx, Value::Object(Some(fs)));
    return Ok(fs);
}
```

`p57_alloc_default_filesystem` (line ~14319) mints the REAL
`sun/nio/fs/LinuxFileSystem` via `p57_alloc_default_filesystem_unix`
(the JDK's own constructor, run for real) **only if** `ensure_class_initialized`
on that class succeeds at that exact moment; otherwise it silently falls back
to the pre-2026-09-17 **synthetic** stand-in — a 4-field object stamped with
the *abstract* `java/nio/file/FileSystem` class, which structurally cannot
have a `needToResolveAgainstDefaultDirectory()` method (that method belongs
to the concrete `UnixFileSystem`/`LinuxFileSystem` subclass, not the abstract
supertype):

```rust
None if !cfg!(windows) && ctx.is_jdk_only() => {
    match p57_alloc_default_filesystem_unix(ctx)? {
        Some(fs) => Ok(fs),
        None => p57_alloc_default_filesystem_synthetic(ctx),   // <- permanently cached if reached first
    }
}
```

If the FIRST-EVER caller in a given process happens to hit this path at a
moment when `sun/nio/fs/LinuxFileSystem` cannot yet initialize (some
transient ordering/timing condition, not yet identified), the synthetic
object gets minted, cached as *the* permanent singleton, and every later
caller — even javac running seconds afterward, even though the class could
now init fine — gets handed the same wrong, abstract-shaped object. This is
mechanically identical in shape to the already-fixed Windows "two doors"
defect (`W8-C4-3-default-filesystem-two-doors.md`): a real invariant
(`FileSystems.getDefault()` is a process-lifetime singleton with HotSpot's
own guarantee) implemented as "trust whatever the first attempt produced,"
with no self-healing if that first attempt was unlucky.

**Direct evidence this is the actual mechanism, not just a plausible story**:
the `postfix` run's own failing logs for `TestCases` and `TestAlterSchemaRename`
each print, verbatim, in the "JVMS 6.5 uninstantiable-receiver census" line:

```
java/nio/file/FileSystem (abstract, requester=native-builtins/src/phases_late/nio_file.rs:14313)
```

— i.e. that specific run really did mint and hand out the abstract synthetic
`FileSystem`, from exactly the code path this doc describes, confirming the
hypothesis against the actual failing artifact rather than just the source.

## Reproduction attempts this session — all negative

Built `/data/wt-h2-fs-cluster-20260922` (branched fresh off `dev` after the
buffer-race fix landed, so functionally identical `nio_file.rs` to the
`postfix` binary that failed) with temporary debug tracing
(`CRATONVM_DBG_DEFFS=1`, since reverted/not committed) on
`p57_alloc_default_filesystem_unix` and `p57_default_filesystem_singleton`,
printing every mint/cache-hit and the class of the object each call returns.
None of the following reproduced the `NoSuchMethodError`, or ever observed a
`FIRST MINT` returning anything other than the real `sun/nio/fs/LinuxFileSystem`:

1. **Isolated single-class runs**, `TestAlterSchemaRename` alone, 4 repeats.
2. **16-way parallel invocations** of the same class, direct (bypassing the
   suite runner's one-JVM-per-class-sequential loop) to force genuine 8-core
   contention from the SAME workload — all 16 succeeded, real mint every time.
3. **Full 218-class sequential run**, matching the `postfix` run's exact
   category/order/timeout, tag `fsdbgfull` — `TestAlterSchemaRename`,
   `TestTriggersConstraints`, `TestView` all PASS (see below for `TestCases`,
   which behaved differently but NOT via this bug).
4. **Deliberately induced host contention**: touched a low-level source file
   to force a multi-minute `cargo build --release` in the background, and
   — coincidentally, three OTHER concurrent Claude sessions on this shared
   Azure host were also mid-`cargo build` at the same time (confirmed via
   `pgrep -af cargo`, load average climbed well past the idle baseline). Six
   repeats of the 4-class cluster run during this genuinely busy window: all
   4 classes PASS in all 6 repeats.

**One related-but-distinct finding surfaced by attempt 4**: under that same
heavy-contention window, `TestCases` (not originally in the 4-class cluster,
but confirmed above to share the same `NoSuchMethodError` in the `postfix`
run) instead HANGs at the full 300s cap, deterministically, in all 6 repeats
and in the `fsdbgfull` background run. Its own debug trace shows the real
`LinuxFileSystem` minted correctly on the first call and 10,000+ subsequent
cache hits against that same real object throughout the hang — i.e. this
HANG is unrelated to the FileSystem bug; the class is just slow (an ordinary
throughput cliff, the same shape as `TestBtreeIndex` below), and the
contention this session induced to chase the `NoSuchMethodError` made that
cliff worse, not the target defect itself.

## What this means for the next attempt

The mechanism is real (matches the failing artifact's own diagnostic line)
but the triggering condition is rare and did not recur under either light
parallel load or heavy genuine multi-session build contention. A speculative
"upgrade the cached singleton from synthetic to concrete on a later call"
fix was considered and deliberately NOT implemented: without a live
reproduction to verify against, such a change carries real identity-safety
risk (any early caller that captured the synthetic reference and later
compares it with `==` against a fresh `FileSystems.getDefault()` would break)
and should not be landed unverified. The next session attempting this should
either: (a) keep `CRATONVM_DBG_DEFFS`-style tracing on through a long,
naturally-occurring full-suite run (or several) until it recurs organically,
or (b) instrument `ensure_class_initialized("sun/nio/fs/LinuxFileSystem")`'s
actual failure reason (not just ok/err) so that if it ever IS reached with
`err = Some(...)`, the specific blocking dependency is known immediately
rather than needing another repro cycle.

## `TestBtreeIndex` — reclassified, not part of this cluster

The census's other 2026-09-22 open item, `TestBtreeIndex` HANG (300014ms,
HotSpot 2.5s), is **not a new or unexplained defect**. It is already
documented and triaged in
[`hangs-true-vs-perfcliff-RESOLVED-20260821.md`](../../internal/fixed-suite-bugs/h2-suite-bugs/hangs-true-vs-perfcliff-RESOLVED-20260821.md):
"Genuine perf-cliff, still capping, zero OOM signature, actively computing —
matches its own 2026-08-10 stack-sample finding ('progressing, not stuck,'
25.1x — reverified fresh)." Confirmed again this session by reading the
`postfix` run's own `TestBtreeIndex` log: it is making steady forward
progress through repeated `testIndex <seed>` fuzz iterations the whole 300s,
not stuck. No fix is appropriate here — it is general interpreter/JIT
throughput, not a discrete bug.

## Related
- `nonpassed-classbyclass-census-20260922.md` — source census.
- `bug-h2-util-gettemporarydirectbuffer-race-FIXED-20260922.md` — the sibling
  fix landed the same day, for a different 11-class cluster; that one WAS
  reproducible and IS fixed.
- `W8-C4-3-default-filesystem-two-doors.md` — the analogous, already-fixed
  Windows-side defect this mechanism is structurally identical to.
- `hangs-true-vs-perfcliff-RESOLVED-20260821.md` — `TestBtreeIndex`'s actual
  classification.
