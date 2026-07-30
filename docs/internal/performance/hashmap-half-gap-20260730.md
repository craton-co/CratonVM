# CratonBench HashMap half-gap closeout (2026-07-30)

## Acceptance

Goal: reduce the `CratonBench hashmap` wall-time gap between CratonVM and
HotSpot by at least 50%, with every run returning the exact checksum
`1549999915000000`.

```text
old_gap = median(baseline) - median(HotSpot)
new_gap = median(final)    - median(HotSpot)
gap_reduction = 1 - new_gap / old_gap
```

Baseline is `origin/dev` at `9ac1feffe`, the commit this branch was last
merged from.

## First: the recorded starting point was stale by ~6x

`BENCHMARK.md` carried `HashMap (10M put/get) — 1,039 / 22,077 ms, 21.2x`
(2026-07-25) and an OPEN item calling the regression "CONFIRMED and bounded to
`a36b9d121..e57f0bc7d`". **Neither reproduces.** Measured on this host before
touching anything, isolated fresh processes, `-Xmx8g`, alternating arms pinned
to **cpu 15**:

| n | HotSpot JDK 25 | CratonVM `dev` @ `9ac1feffe` | ratio | BENCHMARK.md's recorded value |
|---|---:|---:|---:|---|
| 1,000,000 | 66 ms | 427 ms | 6.5x | 3,084 ms / 42.7x |
| 10,000,000 | 997 ms | 3,523 ms | 3.53x | 22,077 ms / 21.2x |

427 ms at 1M is within noise of the 434 ms that the same document records for
the *healthy* `a36b9d121` (07-18) binary, so the bisect range it recommends has
nothing left in it. 3,523 ms at 10M is also consistent with the 4,300 ms
perf-gate baseline that the same document declares unreproducible.

**The cause of the historical reading is not established** — only its
non-reproducibility is. The obvious candidate, the cpu-13 contention that
`BENCHMARK.md` warns about two paragraphs below the numbers it invalidates,
was tested directly and did not reproduce any difference (see "The cpu-13
hypothesis" under Performance evidence). Every measurement in this document is
pinned to cpu 15, with `mpstat -P ALL` confirming that core is 90-100% this
process's.

That correction is applied to `BENCHMARK.md` and `README.md` as part of this
change. It is a *documentation* fix — no code in this change caused the stale
numbers and none fixes them.

## Root cause

With the real starting point established, `perf record -F 299 --call-graph
dwarf` over the 10M phase showed the kernel is fully JIT-compiled (the OSR body
reaches `jit_hashmap_put_direct` / `jit_integer_value_of_direct` — no
interpreter dispatch symbols appear at all), and that the time went to two
different kinds of waste.

**1. The Integer-keyed dense overlay maintained a second full hash table.**
`DenseIntEntries::note_fresh_insert` — four field updates and one
`FxHashMap<i32, u64>` insert — was **19.7%** of the phase on its own, second
only to its caller. That map existed solely so
`keys_in_java_hashmap_order` could reproduce real bucket-chain iteration order.
For the dominant shape — monotonically inserted non-negative keys — insertion
sequence *equals the key*, so the table was storing a value it could derive.
Its 10M cache-missing stores also showed up a second time in the caller: the
mutex unlock's `xchg` (a full store-buffer drain) carried **89.7%** of
`try_hm_int_fast_put`'s own samples, which is what a barrier standing in front
of ten million pending cache-missing stores looks like.

**2. Per-object fixed taxes, each a call/return pair around a no-op.** Once
the overlay stopped dominating, the profile flattened into small costs that
every object touch pays:

| symbol | share of phase | what it was doing |
|---|---:|---|
| `GenerationalHeap::get_header` | 7.0% | out-of-line call per header read; both diagnostics it hosts are default-inert |
| `record_object_ref_payload` | 6.1% | two-level provenance bitmap walk per `ObjectRef` construction |
| `jit_checkcast` + `jit_typecheck_resolve` + `str::from_utf8` | 4.5% | re-deriving a monomorphic site's answer, including UTF-8 validation of the class name, every call |
| `enforce_single_os_thread` | 1.4% | emitted as a real call although disarmed |
| `a2dbg::record` | 1.2% | seven-argument call whose first statement is `if !enabled() { return }` |

## Fix

Inherited from the branch's earlier commits (`806ac07cd`, `a4dfc55fc`):

- `DenseIntEntries` derives the insertion sequence of a dense key from the key
  itself and keeps a `dense_seq_overrides` side table for only the out-of-order,
  migrated or reinserted keys. A sequential generated-ID map leaves it empty.
  The sparse arm carries its sequence inline in the entry instead.
- A sequential-append fast path: when the key is exactly the next dense slot and
  no sparse entry can own it, push straight onto the vector, skipping `resize`,
  the occupied-slot probe and the sparse lookup.

Added by this session:

- **The three arena-membership probes that `806ac07cd` had also dropped are
  restored** (`78090431b`). See "Correctness" below.
- `GenerationalHeap::get_header` is inline; both gated diagnostic bodies moved
  to a `#[cold] get_header_diagnostics`.
- `record_object_ref_payload` keeps a per-thread `(4 KiB block, granule bits
  known set)` memo. Recording is idempotent and the bitmap never clears, so a
  remembered bit means the global store already happened — and consecutive TLAB
  allocations share a block for roughly a hundred objects. The steady state is
  two shifts, a compare and a bit test.
- `enforce_single_os_thread` is `inline(always)` with the armed branch outlined;
  `a2dbg::record` likewise.
- `jit_checkcast` / `jit_instanceof` gained a move-to-front positive-answer memo
  keyed on `(vm, site name ptr/len, receiver class id, lenient)`, consulted
  *ahead of* the UTF-8 validation. Its soundness argument is
  `JIT_SUBTYPE_POSITIVE_CACHE`'s — only `true` is memoized, because a class's
  supertype set is fixed at definition while a `false` can be observed before
  the hierarchy is fully populated — plus two receiver conditions: no class
  redefine in flight, and the receiver must not be an array (a reference
  array's header stores its *component* class id, which the array branch of
  `jit_typecheck_resolve` never consults).

## Correctness

### The inherited safety relaxation, split

`806ac07cd` removed `vm.mem.heap.is_object_address` from four places on the
grounds that the verifier and the call site's oop map already prove the value
is a live oop. That argument holds for exactly one of them:

* **Kept removed** — the exact-`HashMap` receiver in `jit_hashmap_get_direct`
  and `jit_hashmap_put_direct`. `jit_hashmap_receiver_is_exact` has already
  read the receiver's class word through a raw `std::ptr::read` by that point,
  so a later probe cannot prevent a dereference that has already happened.
* **Restored** — `jit_integer_int_value_direct`'s receiver,
  `jit_hashmap_get_direct`'s key, and `jit_hashmap_put_direct`'s key and value.
  Each of those is dereferenced for the *first* time downstream (`get_field`,
  and `unbox_wrapper`'s `class_id_of`). The alignment and canonical-address
  tests that remain reject wild bit patterns but accept any plausible-looking
  non-object address, so the probe was the only thing between a stale or
  fabricated argument and that dereference.

The restoration was measured, not assumed: four interleaved cycles, cpu 15,
10M, medians 1,976 ms without the probes and 2,037 ms with them — about 3% of
wall time, or 6% of the remaining gap. That is cheap insurance and it is kept.

### Differential probes

Two new probes, both diffed against a real JDK 25 run on the same host.

`probes/HashMapIterationOrderProbe.java` is the decisive test for the dense
overlay's sequence derivation. The overlay stores entries in an ascending-key
vector, so `keys_in_java_hashmap_order` has to *reconstruct* JDK bucket order
from `(bucket, first-insertion sequence)`; deriving that sequence from the key
is only correct if every shape where they differ is covered. The probe prints
`keySet` / `values` / `entrySet` order for ascending inserts, descending
inserts, a sparse-to-dense migration (key 2048 inserted before the frontier
reaches it), a value update (must NOT move the entry within its chain),
remove-then-reinsert (must move it), interleaved parity, negative keys, and a
gap-heavy key set.

```text
--- ORDER: hotspot vs final ---
IDENTICAL
--- ORDER: base vs final ---
IDENTICAL
```

`probes/TypecheckAnswerMemoProbe.java` covers the checkcast/instanceof memo's
key assumptions: a polymorphic site, an array receiver versus its component
type (`String[]` and `String` present the same receiver class id), two sites
alternating, the never-cached negative arm, a failing cast after 200,000
successes at the same site, and null. HotSpot, the `dev` baseline and the final
binary all print the same 7 PASS lines.

`bench/HmSemanticsProbe.java` (overwrite, `put` return value, absent key, null
key, null value, size): HotSpot `3750078215`, final `3750078215`, `MATCH=true`.

### Unit tests

Run on the Azure host at the final commit, with plain `origin/dev`
(`c8da3d918`) as a control for every failure.

| target | this branch | control (`origin/dev`) |
|---|---|---|
| `cratonvm-types` lib | 418 passed, 0 failed | 417 passed, 0 failed |
| `cratonvm-gc` (all targets) | 914 passed, 0 failed | — |
| `cratonvm-jit` (all targets) | 1,248 passed, 0 failed | — |
| `cratonvm-native-collections` lib | 85 passed, 1 failed | same 1 failed |
| `cratonvm-vm` lib | 2,448 passed, 6 failed | **same 2,448 / same 6** |
| `cratonvm-types` `flag_surface` | 1 passed, 1 failed | same 1 failed |

**Every failure is pre-existing on `dev` and none is touched by this change.**
The `cratonvm-vm` control reproduces the identical six test names and the
identical pass count. The eight are:

* `types` `flag_surface::inventory_matches_the_checked_in_surface` —
  `types/tests/flag-surface.txt` is checked in with a UTF-8 BOM, so its first
  entry reads as `\u{feff}CRATONVM_ACTIVE_PROFILES_IDENTITY_TRACE`. Present on
  `origin/dev`; introduced by `e3cb2ab17`.
* `native-collections` `tests::fork_join_pool_await_quiescence_registered` —
  also fails at `9ac1feffe`.
* `vm` `jit::conservative_roots::tests::shadow_window_is_recovered_from_a_live_compiled_frame`,
  `jit::skip_list::tests::{classify_complex_ctor_with_putfield,
  complex_ctor_keeps_constructor_ban,
  generated_proxy_class_is_jit_eligible_after_proxy_jitcall_1_removal}`,
  `runtime::interpreter::tests::{b3_gate_scans_full_production_body_of_interpreter,
  hot_files_have_no_production_panics}`. The last one ratchets a panic-site
  count for `jit/src/x64.rs` at 16, and `dev`'s `183aa1562` (merged hours
  earlier) added ~42 lines to that file — the first thing to check when these
  are triaged.

The new `types::value::tests::provenance_memo_does_not_suppress_records_within_a_word`
pins the one way the provenance memo could be wrong: a second address in the
same 4 KiB word being answered from the memo and never reaching the bitmap.

## Performance evidence

The shared 16-vCPU host never reached the perf gate's required load below 2
during this session (other sessions' work kept the 1-minute average between 3.4
and 19). Acceptance therefore used balanced interleaved runs pinned to cpu 15,
fresh processes, `-Xmx8g`, no discarded samples, with `mpstat -P ALL`
confirming cpu 15 at 90-100% for the measuring process. Load is recorded at
every process start.

### Acceptance — `CratonBench hashmap`, the gate's own harness

Five reps, all three arms in each rep, cpu 15, taken in the session's quietest
window (load 3.38-3.80). All 15 checksums `1549999915000000`.

| Arm | Five samples (ms) | Median |
|---|---|---:|
| HotSpot JDK 25 | 975 / 997 / 1,017 / 1,019 / 1,032 | **1,017** |
| Baseline `dev` @ `9ac1feffe` | 3,671 / 3,694 / 3,776 / 3,821 / 3,846 | **3,776** |
| Final | 1,768 / 1,777 / 1,780 / 1,853 / 1,859 | **1,780** |

```text
old_gap = 3776 - 1017 = 2759 ms
new_gap = 1780 - 1017 =  763 ms
gap_reduction = 1 - 763 / 2759 = 72.3%
```

The ratio to HotSpot goes from **3.71x to 1.75x**. The requested reduction was
50%; this exceeds it by 22.3 percentage points.

### Corroboration — same harness at load 10-12

The identical series run earlier, while the host sat at load 10-12. Every
absolute is inflated; the gap reduction is not.

| Arm | Five samples (ms) | Median |
|---|---|---:|
| HotSpot JDK 25 | 1,052 / 1,058 / 1,062 / 1,129 / 1,155 | 1,062 |
| Baseline | 4,037 / 4,064 / 4,065 / 4,094 / 4,748 | 4,065 |
| Final | 1,853 / 1,866 / 1,870 / 1,887 / 1,932 | 1,870 |

`gap_reduction = 1 - (1870 - 1062) / (4065 - 1062) = 73.1%`.

### Corroboration — `bench/HashMapOnly` 10M

The same kernel through the standalone harness, five cycles, while the host
load climbed from 12.6 to 18.6 (hence the wider spread — the interleaving is
what keeps this comparable). All 15 checksums exact.

| Arm | Five samples (ms) | Median |
|---|---|---:|
| HotSpot JDK 25 | 1,070 / 1,359 / 1,575 / 1,583 / 1,642 | 1,575 |
| Baseline | 5,043 / 5,409 / 5,429 / 6,078 / 6,092 | 5,429 |
| Final | 2,112 / 2,535 / 2,768 / 2,979 / 3,233 | 2,768 |

`gap_reduction = 1 - (2768 - 1575) / (5429 - 1575) = 69.0%`.

An earlier five-cycle pass at load 2.5, before the arena probes were restored,
read HotSpot 997 / baseline 3,523 / candidate 1,799 — 68.1%. Every methodology
tried lands between 68% and 73%.

The repository's own gate agrees: `run-cratonbench-gate.sh --cpu 15 --reps 5
--phases hashmap` against the final binary reports `PASS median 1767ms <=
1963ms` at load 3.05, enforcing the exact checksum on all five runs.

### No regression elsewhere

Four of this change's five edits are on VM-wide paths (`get_header`, the
provenance bitmap, the `ObjectRef` tripwire, `checkcast`/`instanceof`), so the
other phases matter. Interleaved base-vs-final, cpu 15, medians of 5, plus a
full seven-phase sweep of 3 reps earlier in the session. **All 42 + 30
checksums exact.**

| phase | baseline median | final median | delta |
|---|---:|---:|---:|
| bintrees (d=18, the *anchored* row) | 1,635 ms | 1,641 ms | +0.4% |
| stringregex (100K) | 229 ms | 223 ms | −2.6% |
| sieve (100K x 20,000) | 6,245 ms | 5,938 ms | −4.9% |

Arithmetic, fib and matrix were measured at 3 reps in the earlier sweep and
all sat inside that sweep's within-arm spread.

### The cpu-13 hypothesis, tested and NOT confirmed

`BENCHMARK.md` warns that the gate's default pin is cpu 13 and that concurrent
sessions pin their own CratonBench there too, which would explain a "quiet
host" reading being several times too slow. That is a plausible story for the
stale 22 s figure, so it was tested rather than asserted — the same baseline
binary, same phase, alternating cpu 13 and cpu 15:

```text
cpu=13 load=11.13 | 4131 ms
cpu=15 load=10.64 | 4063 ms
cpu=13 load=10.19 | 4004 ms
cpu=15 load= 9.77 | 4042 ms
```

No difference. **The hypothesis is not supported by this measurement** (cpu 13
simply happened to be free), and the cause of the historical 22 s reading is
therefore *not established* — only its non-reproducibility is. `BENCHMARK.md`
is corrected to say exactly that.

### Perf-gate baseline

The `hashmap` row is re-anchored from 4,300 ms to **1,800 ms**, still
`provisional`. It is the acceptance median (1,780) rounded up, and the gate's
own run reproduces it (1,767 at load 3.05, budget 1,890 at the default 5%
tolerance). Caveat stated in the TSV and repeated here: load 3.4-3.8 is above
the gate's own `--max-load 2`, so this is a slightly loose ceiling and should
be tightened on a genuinely quiet host. Leaving it at 4,300 was the worse
option — the gate would no longer notice a 2.4x regression.

## What is left

For whoever picks this up. The post-change profile is:

| symbol | share |
|---|---:|
| `jit_integer_value_of_direct` | ~20% |
| the unbox path (`get_header` + `fast_unbox_primitive_wrapper` + `unbox_wrapper` + `get_field`) | ~21% |
| `try_hm_int_fast_get` + `try_hm_int_fast_put` | ~14% |

The first is the 20 million `Integer` boxes the kernel allocates; closing it
means emitting an inline TLAB bump for `Integer.valueOf` in JIT codegen rather
than calling a helper. The third is dominated by the per-operation shard
`Mutex` — in the *pre-change* profile the unlock's `xchg` carried 89.7% of
`try_hm_int_fast_put`'s samples, mostly as a store-buffer drain in front of the
cache-missing stores that are now gone; what remains is the two atomics
themselves. Removing them needs either a thread-biased lock or a
stable-address overlay memo with an epoch. Both are materially riskier than
anything here and neither is needed for the stated goal, so neither was
attempted.

## Artifacts

All binaries are fat-LTO release builds from the isolated task worktree
`/data/data/wt-hashmap-halfgap-20260730` on the Azure EPYC bench host, with
task-unique names under `/data/data/bin-hmhg/`.

```text
b8cf483b8294f5d4a07301d72077a8dabdd3fe944050a126842bb7e7a76a3226
  cratonvm-hmhg-baseline-9ac1feffe        (origin/dev, the acceptance baseline)

f094778586ae1b51b7e2296fc3f1099e483766888529eaa9bcc526042be6f47f
  cratonvm-hmhg-final-5f0cc41b2d          (the acceptance final)
```

`5f0cc41b2` is Rust-identical to the merged tip: the commits after it add only
`probes/*.java` and this document.

Two intermediate binaries were kept for the attribution in "Correctness":
`cratonvm-hmhg-cand1-137abb8b3` (inherited work, arena probes still removed)
and `cratonvm-hmhg-cand2-78090431be` (probes restored, per-object taxes not yet
addressed).
