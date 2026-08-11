# H2 under `-XX:+UseG1GC` — SIGSEGV at a byte-identical heap address — **FIXED**

**Status:** FIXED (2026-08-11) by
`fix(g1): the post-evacuation-failure rset walk accepted any region-resident word as an object`
on `fix/g1-h2-sigsegv-and-aot-cglib-20260811`. The crash is
`G1Collector::record_outgoing_rset_edges` reading an `ObjectHeader` out of an
address it only knew was *inside some region's span*, and then walking the slots
that header claimed.

## The site, named

The page's step 1 was "symbolize". Against its own binary
(`cratonvm-h2-g1-postmerge2`), with the fault PC taken relative to the FIRST
mapping of the image (`prev:`, file offset 0) rather than the `here:` line —
`0x6238e2eaaab0 - 0x6238e25b0000 = 0x8FAAB0`, and
`0x583a2aaddaab - 0x583a2a1e3000 = 0x8FAAAB` for the other class:

```
$ addr2line -f -C -i -e cratonvm-h2-g1-postmerge2 0x8FAAB0
<…G1Collector as …GarbageCollector>::collect_garbage
load                            core/src/sync/atomic.rs:2870
read_value_atomic               types/src/value.rs:1571
for_each_flat_object_reference  gc/src/g1.rs:105
record_outgoing_rset_edges      gc/src/g1.rs:2315
retry_after_evacuation_failure  gc/src/g1.rs:2244
collect_garbage                 gc/src/g1.rs:9070
```

(The page's own offset arithmetic — `0x27caab0` / `0x27caab` — has a typo in the
first value and is against the `here:` base; the two real offsets are `0x8FAAB0`
and `0x8FAAAB`, five bytes apart, which is the same "one routine, adjacent
instructions" conclusion the page drew.)

## Root cause

`retry_after_evacuation_failure` hands `record_outgoing_rset_edges` the
self-forwarded keys of a FAILED evacuation's pointer map (`identities()`:
entries where `k == v`). That function asked exactly one question before
dereferencing:

```rust
let Some(src_idx) = self.lookup_region_for_addr(obj_addr) else { return; };
let obj_ptr = obj_addr as *mut u8;
// Kept objects are ordinary (humongous regions never enter a CSet),
// so flat payload reads are in-bounds.
let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
```

`lookup_region_for_addr` answers "is this word inside `[base, base + region_size)`
for some region" — not "is this an object", and not even "is this below the
region's allocation cursor". The comment asserting the reads are in-bounds is an
invariant the pause's own logs disprove: immediately before both faults, the
sibling walk is on record rejecting addresses out of that same population —

```
[g1] evacuation ref-scan REJECTED a non-object HOLDER (#1): obj=0x2004dc2fbc8
  — walking its slots would have read outside any live region. Skipped; the pause continues.
```

— i.e. `scan_and_evacuate_refs` already had the screen this walk lacked. A
header read at a non-object address yields an arbitrary `num_slots` /
`array_length`, and `for_each_flat_object_reference` then reads upward until it
leaves the mapping.

That is also why `addr=0x20084400000` is byte-identical across independent
processes with independent ASLR bases, which the page correctly refused to call
coincidence: the heap arena is a fixed-base mapping, so a linear walk off its
end faults at the first unmapped page after the arena — a constant of the
mapping, not of the run. Same reason four different classes
(`TestKillProcessWhileWriting`, `TestRandomMapOps`, and — on the 2026-08-10 G1
sweep, at the same address, with `cratonvm-h2-g1` — `TestReorderWrites` and
`TestValueMemory`) all report the same number.

## The fix

Apply the sibling's two guards in `record_outgoing_rset_edges`:

* `candidate_header_is_plausible` on the seed itself (alignment, arena bounds,
  region type, **below the region's cursor**, valid kind/element tags, sane slot
  count). A rejected seed is skipped and counted in the new
  `KEPT_SEED_REJECTED`, so a run can be asked whether the hole was live.
* `holder_walkable_slots` to clamp a reference array's element walk to what the
  holder's region actually holds.

Dropping a non-object seed costs nothing: an address that is not an object has
no outgoing references to remember.

Two unit tests in `gc/src/g1.rs` pin both arms — a seed above its region's
cursor carrying a fabricated one-element reference-array header must record no
edge and must be counted, and a real live seed's cross-region edge must still be
recorded. With the two guards disabled the first test FAILS and the second still
passes (verified, `cargo test -p cratonvm-gc --release kept_seed`), so the pair
is not a vacuous green.

## Measurement

`TestValueMemory` reproduces the fault site in ~20 s per run — far cheaper than
the page's two classes — and was used as the A/B vehicle. Interleaved arms, same
host, same `run-h2-suite.sh` invocation, `--jit on --jdk real`, `-Xmx 1g`, both
binaries built from the same tree (`cratonvm-goal-base` = the branch point,
`cratonvm-goal-g1fix` = one commit later):

| arm | runs | SIGSEGV at `0x20084400000` | `kept-seed … REJECTED` |
| --- | --- | --- | --- |
| pre-fix (`cratonvm-goal-base`, `-XX:+UseG1GC`) | 15 | **4** | n/a (no guard) |
| post-fix (`cratonvm-goal-g1fix`, `-XX:+UseG1GC`) | 15 | **0** | fired in 8 of 15 runs (3, twice 6) |

Every one of the four pre-fix crashes carries the identical fault address
(`addr=0x20084400000`) and the identical image offset (`0x90C7CB`), which `gdb`
places at `collect_garbage+22955`, `core/src/sync/atomic.rs:3904` — the same
innermost line as the page's own binary's inline chain above.

The post-fix guard fires in **8 of 15 runs** — 3 seeds each time, twice 6 — and
always in the same runs where the ref-scan sibling rejects 8-9 holders. That is
the direct evidence that the hole was live on this workload: three seeds per
occurrence that the pre-fix code walked without checking.

(The pre-fix crashing runs all report exactly 3 holder rejections against 6-8 in
the non-crashing ones, because the fault aborts the process partway through
emitting them — the truncation is a symptom of the same pause, not a different
one.)

Additionally, three post-fix rounds over all four historically-affected classes
(`TestReorderWrites`, `TestKillProcessWhileWriting`, `TestRandomMapOps`,
`TestValueMemory`) produced **0 SIGSEGVs**, and
`ApplicationContextAotGeneratorTests` is 40/40 under `-XX:+UseG1GC` on the same
tree, so the guard does not break the healthy path.

## What is NOT fixed, and was never this page's defect

The page said both of its classes "run clean under plain HotSpot". They do — but
they do not pass under CratonVM once the crash is gone, and they did not pass
under the other collectors *before* it either. On the page's own binary,
under **ZGC**, `TestKillProcessWhileWriting` already failed with the same
`OutOfMemoryError: Java heap space` and `TestRandomMapOps` already HUNG at the
300 s cap. Post-fix, under G1, the four classes' remaining faces are:

| class | remaining face |
| --- | --- |
| `TestKillProcessWhileWriting` | `MVStoreException: java.lang.OutOfMemoryError: Java heap space` (the runner's `-Xmx 1g`) |
| `TestRandomMapOps` | `ClassCastException: java.lang.Object cannot be cast to java.lang.Integer` |
| `TestReorderWrites` | `NoSuchMethodError: java.text.FieldPosition.length()I` |
| `TestValueMemory` | passes in 2 of 3 rounds; otherwise an H2 memory-accounting `AssertionError` |

None of these is collector-specific and none is the shared fault site. They
belong to the long-running H2 residual triage
(`bug-h2-suite-residual-fail-triage-FIXED.md`, this folder, which already
carries `TestRandomMapOps` as "heavy fuzz workload, very likely fixed, not
verified to completion"), not here.

## Open follow-up, stated rather than buried

`KEPT_SEED_REJECTED` and `EVAC_HOLDER_REJECTED` are both documented as "expected
to be ZERO", and on this workload both are non-zero. The guard makes the pause
survivable; it does not explain why a self-forwarded pointer-map key is not an
object start by the time the unresolved-kept block runs. The most likely
explanation — `drain_kept_self_forwards` runs a Phase-5 that can reset/retype
regions, so a seed captured before it can be above its region's *new* cursor
afterwards — fits the observed rejection shape but was not established here.
Anyone picking that up should start from the counters, not from a fresh crash.

## Related

- `gc-variant-fullsuite-crashes-hangs-fails-20260810.md` — the sweep this
  surfaced from; note its `addr=0x10` SIGSEGV is a different shape (fault in
  `libc.so.6`, all three collectors) and is not this defect.
- `1ccaf9caf` (`fix(g1): the reachability verifier traversed anything
  region-resident, including payload`) — the same mistake, one week earlier, in
  the `CRATONVM_DBG=g1-dbg-reach` verifier. That one was flag-gated; this one
  was on by default in every G1 run.
- The page's "read first" pointer to a hibernate G1 doc is dropped: that record
  no longer exists in the tree.
