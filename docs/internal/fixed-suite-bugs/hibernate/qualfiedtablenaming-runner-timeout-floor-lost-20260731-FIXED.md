# `DefaultCatalogAndSchemaTest` — the lost runner accommodation is re-implemented (durably), and the class's *real* blocker turned out to be a GC reference-processing corruption, not the timeout

| | |
|---|---|
| **Status** | ✅ FIXED 2026-07-31. Supersedes the OPEN write-up formerly at `docs/known-issues/hibernate/qualfiedtablenaming-runner-timeout-floor-lost-20260731.md`. |
| **Class** | `org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest` |
| **Branch** | `codex/fix-hib-runner-timeout-floor-20260731`, from `origin/dev` @ `a31a8a93f` |

## What the OPEN doc got right, and the part it could not have known

The OPEN doc was correct that the 2026-07-22 "Resolved" section of
[`qualfiedtablenaming-hang-cluster-20260721-FIXED.md`](qualfiedtablenaming-hang-cluster-20260721-FIXED.md)
described a `run-hib.sh` accommodation — a 3600-second per-class timeout floor
plus a forced `--nojit` — that does not exist in the current script, that
`apps/` being wholly gitignored is why it could vanish without a trace, and
that the fix is to re-implement it in a *tracked* location.

What it could not have known, because it inferred the class's behaviour from
the 2026-07-21 diagnosis rather than re-running it to completion: **on current
`dev` this class does not pass in either mode, with any timeout.** The
`HANG`/`rc=124` in the fresh 4548-class run is real, but a timeout floor alone
would only have converted it into a `CRASH`. Both blockers below had to be
fixed before the accommodation could do its job.

This is the [[feedback_verify_known_issue_doc_stated_root_cause]] pattern: a
doc whose *stated* root cause is a subset of the real one, because the
"already-understood, just slow" framing was inherited rather than re-derived.

## Control: HotSpot, same heap, same classpath, same runner

```
$ java -Xmx1500m @common.args -Dcraton.batch=1 CratonRunner \
      org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest
@@RESULT ... found=132 started=132 ok=132 failed=0 aborted=0 skipped=0 ms=119721
```

120 seconds, all 132 tests, empty stderr. The workload fits in the suite's own
`--Xmx 1500m` with room to spare — so neither the OOM nor the segfault below is
an under-provisioned heap, and the historical `started=123` was a CratonVM
artifact, not 9 genuinely-skipped tests.

## Blocker 1 (real VM defect): `--nojit` SIGSEGVs — post-GC weak/phantom referent restore writes through recycled memory

`HIB-WEAKREF-RECYCLE.1`. Reproduced twice, deterministically:

| arm | outcome | dropped out-of-bounds field writes |
|---|---|---|
| `--nojit` #1 | **SIGSEGV rc=139** after ~17 min | 174 |
| `--nojit` #2 | **SIGSEGV rc=139** after ~19 min | 606 |

158 of run #1's 174 drops share one signature, and every backtrace bottoms out
in the same place:

```
gen_heap::set_field: out-of-bounds field write dropped
  index=0 num_slots=0 class_id=ClassId(0) class_name=java/lang/Object
  real_field_count=Some(0) value=Object(Some(ObjectRef{..}))

  4: cratonvm_gc::gen_heap::GenerationalHeap::set_field       gc/src/gen_heap.rs:2745
  5: cratonvm_vm::runtime::interpreter::process_references_after_gc
                                                 vm/src/runtime/interpreter.rs:2559
  6: cratonvm_vm::runtime::interpreter::maybe_gc_forced        interpreter.rs:1693
  8: cratonvm_vm::vm::vm_exec::safe_native_call_impl           vm_exec.rs:904
```

The rest carry garbage receivers — `class_id=ClassId(8471593)`,
`ClassId(4247487416)`, `ClassId(2683438736)`, `class_name=<unresolved>`,
`real_field_count=None`. Line 2559 is the HIB-CV-24 **post-GC weak/phantom
referent RESTORE** pass.

### Root cause

Both referent passes resolve a processor-held Reference address with

```rust
match pointer_map.get(&addr) {
    Some(&a) => a,                                        // relocated survivor
    None if shared.mem.heap.is_addr_live(addr) => addr,   // unmoved survivor
    None => continue,
}
```

and for the Generational heap `is_addr_live` is
`is_old_gen_addr(addr) || is_live_young_survivor(addr)`. `is_live_young_survivor`
is precise (it reads the header word and rejects the zeroed span a sweep
leaves behind), but **`is_old_gen_addr` is a pure address-range test**: freed,
recycled old-generation memory still answers "live". A weak/phantom entry whose
`Reference` object was reclaimed by an old-gen sweep therefore

1. survives `remove_collected(&is_marked)` forever — `is_marked` bottoms out in
   the same range check — and
2. is written to, at slot 0, by **both** referent passes on **every subsequent
   collection**, targeting whatever now occupies that address.

That is exactly why the same handful of addresses recur 158 times.

The sibling `cleared` and `to_enqueue` loops *in the same function* already
carry the guard for this, and say so in their own comment — *"a live
`java.lang.ref.Reference` always has >= 2 instance fields (referent, queue); a
reused/zeroed slot is a bare 0-field `Object` … also covers old-gen reuse after
a major GC"*. The restore pass and `weakref_null_referents_pre_gc` never got it.
The pre-GC pass was the more dangerous of the two: its guard was `>= 1`, so
when the recycled memory happened to hold a 1-field object the null write
**landed silently on a real field** instead of tripping the `gen_heap` guard.

### Fix

- `vm/src/runtime/interpreter.rs` — `weakref_null_referents_pre_gc`: guard
  tightened `>= 1` → `>= 2`.
- `vm/src/runtime/interpreter.rs` — post-GC restore pass: same `num_fields < 2`
  skip the `cleared` loop uses, with a `CRATONVM_DBG_STRAYSTACK` trace line
  matching that loop's.
- `gc/src/reference.rs` — new `retain_shaped_weak_phantom`, called right after
  `remove_collected`, which resolves each entry to its post-collection address
  exactly as the restore loop does and drops it unless a `Reference` still
  lives there. Deliberately scoped to `weak_refs`/`phantom_refs`:
  `finalizer_refs`/`cleaner_refs` hold arbitrary application objects that may
  legitimately declare fewer than two fields, so the shape test is unsound for
  them. Unit test
  `retain_shaped_weak_phantom_prunes_recycled_and_spares_finalizers` pins both
  halves.

The `>= 2` invariant holds in synthetic-JDK mode too: `ref_init_impl`
unconditionally writes `REF_FIELD_QUEUE` (slot 1), and `ref_next_slot` calls the
minimum shape the *"legacy synthetic 2-field shape"*.

### What the fix did and did not do

Measured on the same class, same flags, baseline vs fixed binary:

| | baseline | + HIB-WEAKREF-RECYCLE.1 |
|---|---|---|
| `process_references_after_gc` frames in corrupt-write backtraces | 5 of 5 sampled | **0** |
| corrupt writes declined by the new guards | — | **84** |
| outcome | SIGSEGV | SIGSEGV (still) |

The fix is real and eliminates this writer completely, but the `--nojit` arm
still dies — via a **second, independent** corrupt writer in
`native-collections`' map resize. Root-caused and filed separately as
[`HIB-MAPRESIZE-STALE.1`](map-resize-unpinned-chain-cursors-nojit-segv-20260731-FIXED.md);
not fixed here, deliberately (~50 lines of pin plumbing in a hot native, with a
trap found while prototyping, and a ~50-minute validation cycle — it deserves
its own change, not a fold-in to a harness fix). **Resolved 2026-08-03** — see
that doc's own Follow-up 4/Resolution: the actual writer turned out to be
`OldGen::compact`, not the map-resize natives this section suspected.

## Blocker 2 (real VM defect): JIT-on OOMs on a half-empty heap

The suite's default lane fails differently: `OutOfMemoryError: Java heap space
(anewarray component 6 length 644)` at ~41 minutes, three for three.

`CRATONVM_DBG_GC_OVERHEAD=1` shows the live set plateaued at **554 MB of a
1125 MB cap** — 49 % full, ~570 MB free — with `promoted=0` on all thirty
forced GCs and ~2.8 MB reclaimed per cycle, so the "< 2 % of capacity freed"
streak latches at 8 and the overhead limit turns the next allocation failure
into an OOM. The differential confirms it: with
`CRATONVM_GC_OVERHEAD_LIMIT=0` the same binary ran **85 minutes without an
OOM**, past the point where it otherwise dies.

Underneath, every young collection is diverted to the **non-moving,
non-compacting** sweep (`compiled-frame-oop-not-published`,
`unregistered-jit-frame-on-stack`, `missing-exact-rbp`,
`active-safepoint-map-incomplete`) — the already-OPEN moving-young coverage gap,
here showing up as a correctness failure rather than only a throughput tax.
Filed as `HIB-GCOVERHEAD-HALFFULL.1`, cross-referenced to
[`moving-young-inert-under-jit-throughput-tax-20260730.md`](../../../known-issues/hibernate/moving-young-inert-under-jit-throughput-tax-20260730.md).

> **CORRECTION, 2026-07-31 (same day).** The clause above — "the already-OPEN
> moving-young coverage gap, here showing up as a correctness failure" — is
> **wrong**, and `HIB-GCOVERHEAD-HALFFULL.1` is now
> [FIXED](gc-overhead-limit-spurious-oom-at-half-full-heap-20260731-FIXED.md).
> The OOM was not downstream of the coverage gap. It was an independent
> regression: the non-moving sweep's selective promotion — the young
> generation's only young→old drain under a live JIT frame — was gated on the
> coverage flag, whose meaning had changed underneath it, so young could never
> drain at all. Running the non-moving sweep is a throughput tax; running it
> *with its drain disabled* is what killed the process. With the drain restored
> the class runs to completion (`ok=121 failed=0`) with **zero** forced GCs.
> The moving-young gap remains open and remains a throughput tax — it is why the
> class still takes 105 min against HotSpot's 120 s.

## Where that leaves the class

**It still does not pass**, and this doc does not claim otherwise. What changed
is that the reason is now *known and measured* instead of assumed:

| | HotSpot | CratonVM `dev` + this branch |
|---|---|---|
| JIT on (the suite's lane) | `ok=132`, 119.7 s | OOM at ~41 min, 104/132 tests reached |
| `--nojit` | — | SIGSEGV at ~20 min |

This doc is retired because **its own subject is fixed**: the lost runner
accommodation is back, durably, and the three recommendations it made are all
implemented. The two blockers above are newly discovered, were never part of
this doc's claim, and each now has its own OPEN doc. Recommendation 3 —
"treat a `HANG` on this class as expected" — is superseded: with the floor in
place the class no longer HANGs at 300 s, it runs on and fails for a real
reason, which is the outcome the accommodation was supposed to expose.

## Blocker 3 (the OPEN doc's subject): the runner accommodation

Re-implemented, and this time in a form that cannot silently disappear.

`apps/hib-suite-runner/run-hib.sh` gains two tables consulted in `run_shard()`
before choosing the wall cap and VM flags for a class, loaded from
`apps/hib-suite-runner/class-overrides.tsv`:

```
<fully.qualified.TestClass>   <timeout-seconds | ->   <extra VM flags | ->
```

The timeout is a **floor** — `max(entry, the run's own --timeout)` — never a
cap, so `--timeout 7200` still wins. Extra flags are appended ahead of the
`@common.args` argfile and de-duplicated.

**Both files are force-added past `.gitignore:12 apps/`**, which is the actual
durability fix: a future truncation or stale-backup restore now shows up as an
ordinary `git status` modification instead of vanishing. A new `.gitattributes`
pins them to `eol=lf` — this repo is `core.autocrlf=true`, and a `bash` script
checked out with CRLF puts a `\r` inside every token it terminates
(`TIMEOUT="${TIMEOUT:-300}"` → `"300\r"`, and the first `-gt` against it fails).

Additional guards, all of them "the runner lies quietly" bugs the OPEN doc's
recommendation 2 was really about:

- Every run's mode header and `SUMMARY.txt` print `overrides=N (loaded)`;
  a missing table prints `overrides=MISSING <path>` plus two stderr warnings.
  `run-hib.sh overrides` dumps the live table.
- A non-numeric timeout in the table is rejected **loudly** rather than ignored.
- `HANG`/`CRASH` rows now record the cap that killed them
  (`process-died rc=124 timeout=3600s`), so a `HANG` row can never again be read
  as "stuck" when it merely outran a too-short cap.
- `--no-overrides` disables the table for A/B checks.

### Two further landmines found while validating the runner

Neither is in the OPEN doc, both make the runner report a whole run wrong:

- **The default JDK path no longer exists.** `JDK="${JDK:-C:/Program Files/Java/jdk-25}"`
  — that directory is gone from this box (JDK 25 is under
  `C:/Program Files/Eclipse Adoptium/`). Nothing validated it, so a run that
  forgot to set `JDK=` passed a dead `--java-home` to every fork and recorded
  4548 × `CRASH`. Now autodetected, with a hard fail if nothing is found.
- **The runner was silently CWD-dependent.** Hibernate's own
  `GradleParallelTestingResolver.getWorkerID` reads a worker-id file relative to
  the process CWD, which the forked VMs inherit from the script. Launched by
  absolute path from anywhere but the fixture directory, every class died in
  ~1 s with `FileNotFoundException` → `"An error occurred when computing worker
  ID"` → `ExceptionInInitializerError` in `JdbcConnectionContext.<clinit>` →
  `CRASH`. Confirmed by direct A/B (same class: `CRASH` from the worktree,
  `PASS` in 3.3 s from the fixture dir). The script now pins its CWD, resolving
  caller-relative `--bin`/`--out` against the original directory first.

## Bookkeeping

- `docs/known-issues/hibernate/qualfiedtablenaming-runner-timeout-floor-lost-20260731.md`
  deleted; this file replaces it.
- `qualfiedtablenaming-hang-cluster-20260721-FIXED.md`'s correction banner
  updated to point here, and its "Resolved 2026-07-22" section annotated: the
  runner change it describes is now real again, but its claim that `--nojit` is
  the safe mode for this class was **inverted** on current `dev` until
  HIB-WEAKREF-RECYCLE.1 was fixed.
- `docs/known-issues/hibernate/README.md`'s HANG-triage entry moved to the
  resolved list.

## Out of scope, deliberately

The `MutableBigInteger`/`BigInteger` interpreter quarantine (`41cdfdf94`,
`HIB-BIGINTEGER-AIOOBE.1`/`.2`) is untouched. It is the reason this class is
legitimately slow enough to need a floor at all, and it is tracked on its own in
[`docs/known-issues/jit-bans/jit-skip-list-open-bans.md`](../../../known-issues/jit-bans/jit-skip-list-open-bans.md)
— narrowing it needs the multi-method x64 lowering bug root-caused, which is a
separate piece of work from this doc.
