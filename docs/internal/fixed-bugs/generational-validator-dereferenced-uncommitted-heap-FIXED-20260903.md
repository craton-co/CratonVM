# The generational validator dereferenced reserved-but-uncommitted heap

## Status

**FIXED 2026-09-03.** Retired from
`known-issues/gc/generational-and-g1-mass-abend-under-spring-framework-3gc-20260903.md`,
whose every open question is discharged below — including its central one,
which had the wrong answer.

## What the page originally claimed, and why it was wrong

The Spring Framework suite (2848 classes, fork-per-class) was run once per
collector:

| GC | OK | "ABEND" | TIMEOUT | FAIL | LOADERR | Wall |
|---|---:|---:|---:|---:|---:|---:|
| **ZGC** | 2832 (99.4%) | **0** | 2 | 5 | 9 | 14394s |
| Generational | 1853 (65.1%) | **976** | 9 | 1 | 9 | 20222s |
| G1 | 1932 (67.8%) | **887** | 18 | 2 | 9 | 25576s |

The page read `ABEND` — the harness's label for "exited non-zero with no
recognised crash signature" — as *"the classic signature of an external
SIGKILL, most commonly the Linux OOM killer"*, and spent its whole "not yet
done" list on a memory-contention confound: all three arms had run
concurrently on one 8-core host.

**Every one of those 976 rows was `rc=139`.** SIGSEGV — with a full CratonVM
crash report in stderr and an `hs_err_pid*.log` on disk. Not one was a
SIGKILL, which would have been `rc=137`. The evidence was already in
`crashes.log`, one command away:

```
$ grep -oE '\[ABEND rc=[0-9]+\]' crashes.log | sort | uniq -c
    976 [ABEND rc=139]
```

The confound was refuted by the same data. **867 of G1's 887 ABEND classes
are also Generational ABEND classes — 97.7% overlap.** Host memory pressure
kills whichever process is unlucky; it does not pick the same 867 classes out
of 2848 twice. And 12 of a 13-class sample re-run **alone and sequentially**
still ABENDed. This was deterministic and per-class from the start.

## The harness defect that manufactured the wrong reading

`run-suite.sh:476` classified a run with no `RESULT` line by grepping stderr
for `panicked at|not yet implemented|unreachable|index out of bounds|
EXCEPTION_ACCESS|STATUS_|fatal runtime` — and calling everything else `ABEND`.
CratonVM's own fatal-signal report says `A fatal error has been detected by
the CratonVM Runtime Environment` / `SIGSEGV at pc=`, none of which is in that
list. **The harness could not recognise its own VM's crash report**, and the
exit code — which says "killed by signal N" whether or not anything was
printed — was never consulted at all.

Fixed on the host (`apps/spring-suite-runner/run-suite.sh`, backed up as
`run-suite.sh.bak-20260903`; that tree is not in this repo): key on
`rc > 128` first, name `137` as `KILLED` (the real OOM-killer shape), and add
the VM's own two report strings to the signature list. `ABEND` now means what
it says.

That is the generalisable lesson: **a classifier that cannot see its own
subject's diagnostics converts a crash into a mystery, and a mystery is what
gets explained by whatever confound is nearest.**

## Root cause

`GenerationalHeap::is_object_address` is the function a conservative root scan
asks *before* touching a candidate word — and it was the thing that
segfaulted. Symbolised from the fault pc:

```
cratonvm_gc::gen_heap::GenerationalHeap::is_object_address
  <- cratonvm_vm::jit::conservative_roots::scan_one_frame (conservative_roots.rs:7551)
```

It screened the candidate against the lock-free `region_bounds` mirror, which
publishes `[base, base + Arena::capacity())`. **`capacity()` is RESERVED
address space.** Since `gc/src/reservation.rs` landed, the young arenas are a
reservation committed in 2 MiB granules as the allocator's cursors reach them;
most of `-Xmx` is unmapped at any moment. A stack word that merely *looked*
like a young-gen pointer passed the envelope test and reached

```rust
let kind = unsafe { object_kind_from_tag(cratonvm_types::kind_tag_at(raw)) }?;
```

in an unmapped page. The `SAFETY` comment on that line — *"`raw` was confirmed
inside a mapped heap region above (`in_region`)"* — was the defect, stated in
prose: `in_region` proves *reserved*, never *mapped*.

**ZGC is immune for a structural reason.** Its `is_object_address` is
`self.registry.contains(addr)` — a live-base bitmap. It never speculatively
dereferences, so it cannot fault on an address it was asked about. That is the
whole of the 0-versus-976 gap.

**G1 is not affected on current dev.** Its regions are plain `Vec<u8>` (wholly
mapped) and `is_addr_in_live_region` bounds the candidate by the region's
allocation cursor. Its 887 crashes were on the Sep-2 binary and are gone on
the current tip; the same 13-class sample is 0/13 under G1 here. Old gen is a
`Vec<u8>` too, so slot 2 needs no screen either — only the two young arenas do.

## The A/B that named it, before a line was written

`CRATONVM_GC_RESERVE=0` forces the wholly-committed `alloc_zeroed` fallback
store, byte for byte, with no rebuild:

| arm | `ComponentScanParserTests` | `ComponentScanAnnotationIntegrationTests` |
|---|---|---|
| default (`RESERVE=1`) | **SIGSEGV** | **SIGSEGV** |
| `CRATONVM_GC_RESERVE=0` | OK (8/8) | OK (25/25) |

A kill switch that removes the crash *and* names the mechanism is worth more
than any amount of reading, and this one already existed.

## The fix

Publish each young arena's per-granule commit bitmap beside its bounds, and
screen the candidate's whole extent against it before the first dereference.

* `Reservation::granules` becomes `Arc<[AtomicU64]>` so a reader can consult
  it without the `Mutex<Arena>` that owns the reservation, and so a `grow_to`
  that swaps the whole reservation out leaves a holder on stale-but-mapped
  words rather than freed memory.
* `GenerationalHeap` gains `commit_bits` (a `(ptr, word count)` mirror) and
  `commit_bits_hold` (the owning `Arc`s), published from
  `store_region_bounds_locked` — the same call, the same STW discipline, so
  the bounds and the bitmap cannot disagree about which arena a slot names.
* `is_object_address` screens `HEADER_SIZE` bytes before reading the header
  and the object's full claimed extent before returning `Some`. A 16-byte
  header eight bytes short of a granule boundary straddles two granules, so
  the screen is over a range, not a point.
* `decommit_granules` now clears the bits **before** the syscall. Unmapping
  first left a window in which a lock-free reader was told a granule was
  backed after it had been handed back — a SIGSEGV, not a wrong answer.
  Clearing first makes that window fail-safe: a reader declines a granule that
  is still mapped, and declining costs nothing.

**Declining is always safe, and that is why this fix has no false-negative
risk.** The screen can only refuse an address in a granule no allocator ever
committed, and no live object can be in one.

### The second face: a garbage `Monitor *`

The other crash site in the same sweep was
`cratonvm_vm::threading::monitor::displaced_hash_from_mark`, reading
`mark = 0xd5ee` — a plain small integer whose low two bits happen to be `0b10`
— as an inflated-monitor pointer, and dereferencing `0xd5ec + 0x46`. That is
what a conservative root scan looks like *downstream* when it accepts a false
object base: the marker is handed sixteen arbitrary heap bytes as a header,
and one word in four is INFLATED-tagged.

`monitor_ptr_from_mark` now refuses a payload that is not
`align_of::<Monitor>()`-aligned or that lies in the first page, with a `const`
assert pinning the alignment the screen depends on. `0xd5ec` is 4-aligned, so
the alignment test alone refuses it. Refusing costs the caller nothing:
`displaced_hash_from_mark` answers `0`, which is already its answer for every
non-INFLATED word.

## Verification

Same host, sequential, one collector at a time — the isolated re-run the
original page asked for and never got.

| arm | crashes | fails |
|---|---:|---:|
| Generational, **before** the fix (13-class sample) | **3** | 0 (masked by the crashes) |
| Generational, `CRATONVM_GC_RESERVE=0` | 0 | 2 |
| Generational, **after** the fix | **0** | 2 |
| G1, after the fix | 0 | 2 |

The fixed arm reproduces the kill-switch arm exactly, including which two
classes fail and how.

**140 classes drawn from the 976-strong Generational ABEND list, re-run one at
a time on the fixed binary: 130 OK, 10 FAIL, 0 CRASH.** All ten FAILs
reproduce identically under ZGC, so they are collector-independent test
failures that the crash had been hiding, not residuals of this fix.

A full 2848-class Generational sweep on the fixed binary, alone on the host,
was started the same day; its counts belong beside this table when it lands.

## What the original page asked for, and where each answer is

| the page's "not yet done" | answer |
|---|---|
| Sequential, isolated single-collector reruns | Done. Still ABENDs alone ⇒ the confound is refuted, not the defect. |
| Whether `dmesg` has OOM records | Moot. `rc=139`, not `137`; nothing was OOM-killed. |
| One concrete ABEND'd class's failure mode | `is_object_address` faulting on an uncommitted granule, symbolised above. |
| Whether the ABEND'd classes cluster | They do — 97.7% shared between Generational and G1, which is what a per-class mechanism looks like and what host pressure does not. |

## Related

The `displaced_hash_from_mark` hardening is the same family as
`is_object_address`'s own Family-A comment — "an interior 16-byte `Value` cell
of a live `Object[]`" that decodes as a plausible header. Both are a
conservative scan's false positive being trusted one layer down.

## Independent pre-fix reproduction on two more codebases (2026-09-04)

Two more full-suite, 3-GC-sharded runs, on a **pre-fix** binary (dev tip
`d9d3eb336`, built 2026-09-03 ~19:56 UTC — after this fix's commit timestamp,
but the fix was sitting on this unmerged branch, not yet in `dev`):

| Suite | Generational CRASH | G1 CRASH | ZGC CRASH |
|---|---:|---:|---:|
| H2 Database (218 classes) | 14 | 2 | 5 |
| Spring Boot (1991 classes) | **347 (17.4%)** | 0 | 1 |

All 361 Generational crashes are `rc=139` with `addr2line` resolving the fault
pc to `<cratonvm_gc::gen_heap::GenerationalHeap>::is_object_address` — same
function, same defect, two more codebases that share no code with Spring
Framework or with each other. This is independent, cross-project confirmation
of the mechanism above, at a scale (361 crashes, two full suites) that
complements the 140-class sample and the pending full Generational sweep.

On the G1-affected-or-not question: this pre-fix binary shows a small,
nonzero residual (2/218 on H2) rather than Spring Framework's 887/2848, and
zero on Spring Boot (0/1991). That's consistent with **the same defect being
present on unfixed G1 too, but far more workload-sensitive** than on
Generational — rare enough to miss entirely on some suites — rather than
evidence G1 needed a separate fix. Not proven either way; the isolated 13/140
-class-style resample this doc already ran for Generational hasn't been
repeated for G1 on H2/Spring Boot's specific classes.
