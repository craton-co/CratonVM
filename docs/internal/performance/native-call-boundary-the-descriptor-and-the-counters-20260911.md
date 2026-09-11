# The compiled native-call boundary: a descriptor parsed three times, and three `lock xadd`s

2026-09-11. Azure EPYC bench host. Follow-on to
[`string-regex-per-element-constant-20260911.md`](string-regex-per-element-constant-20260911.md),
which ended by naming the JIT→native boundary as the next lever and did not
pull it.

## Where the boundary's cost actually is

Two `#[ignore]`d measurement harnesses already lived in the tree —
`vm_exec.rs::native_funnel_profile` and
`jit/helpers.rs::jit_native_dispatch_profile`. Run them
(`cargo test --release -p cratonvm-vm --lib -- --ignored --nocapture`) and the
boundary stops being a guess.

**The funnel** (`safe_native_call_impl`), one object argument, ~26 ns:

| component | ns |
|---|---|
| the callback itself, no funnel | 0.2 |
| `heap.load_and_forward(obj)` — **per object argument** | 4.6-6.1 |
| `NativeStateSpan` enter + restore | 3.9-5.0 |
| **the two `INLINE_NATIVE_ARGS` scratch arrays** | 2.7-3.8 |
| `young_spill_pressure()` | 1.7-2.0 |
| `catch_unwind` | 1.6-3.3 |
| pin push + truncate | 0.6-1.3 |
| `native_diag_mask()` | 0.6 |
| `stw_requested` load, `disable_jit()`, JNI drain, `native_oom` | < 0.5 each |

**The JIT side** (`try_jit_site_cached_native_dispatch` and the dispatcher
around it):

| component | ns |
|---|---|
| `forward_jit_reference_args`: receiver only, `()I` | 6.9-9.9 |
| `forward_jit_reference_args`: receiver + one object | 20.4-34.3 |
| `decode_dispatch_values_into`: receiver only, `()I` | 4.2-5.7 |
| `decode_dispatch_values_into`: receiver + one object | 12.4-19.2 |
| `heap.is_object_address` | 3.0-4.7 |
| `heap.class_id_of_validated` (the same answer, pre-validated) | 1.0-1.3 |
| `NATIVE_SITE_CACHE` probe (hit) | 0.8-1.1 |
| `note_membership_walk` | 4.2-5.5 |
| leaf/site-cached hit counter | 4.1-5.7 |
| `record_invocation` (real id) | 3.9-4.2 |
| all three counters, as one dispatch pays them | 7.3-10.0 |

Two things stand out, and neither is the heap work everybody assumes:

1. **The descriptor is parsed three times per dispatch.**
   `forward_jit_reference_args` walks it to find which arguments are
   references, `decode_dispatch_values_into` walks it again to type each one,
   and `coerce_native_return` scans it a third time for the return tag. The
   string parsing, not the ~3 ns membership walk, is the larger half of both
   of the first two rows above.

2. **Three diagnostic counters cost more than most of what they measure.**
   Each is a `lock xadd` — on x86 a full barrier, not a store — and a
   site-cached dispatch pays all three, 7-10 ns of a ~50 ns dispatch. The
   membership-walk counter costs more than the walk it counts.

## The four changes

### 1. The diagnostic counters stay always-on, and stop being atomic RMWs

Not gated, not sampled — **exact**, and ~13x cheaper. Each thread owns a
`JitCounterBlock` and is its only writer, incrementing with a relaxed
load-add-store, which is an ordinary store with no lock prefix. Readers walk
the block registry and sum. The fields stay `AtomicU64` only so a reader on
another thread is not a data race; no reader ever needed a consistent instant
across counters.

Measured, same run, against `record_invocation` (untouched, as the control):

| | before | after |
|---|---|---|
| `note_membership_walk` | 4.8-5.5 | **0.6** |
| leaf-hit tally | 4.6-5.5 | **0.5-0.7** |
| both, one TLS access | — | **0.6** |
| `record_invocation` (control, untouched) | 3.9-4.2 | 6.0-6.2 |

The control moving is what says the absolute numbers in that run are inflated
and the ratio is the reading: the two tallies went from ~1.2x the control to
~0.1x of it.

### 2. The scratch array is declared, not initialised

`safe_native_call_impl` and `safe_native_call_leaf` both opened with
`let mut inline_forwarded = [Value::Object(None); 8]` — 128 bytes of stores on
every native call, to serve a copy that only happens when the collector
actually moved an argument. Definite assignment lets the fill move into the arm
that reads it, so the ordinary path stores nothing. The file's own comment
already priced the pair at 7.3 ns and said it was paid "whether or not one is
used"; the surviving array measured 2.7-3.8 ns.

### 3. The pre-validated funnel path forwards without re-walking

`safe_native_call_prevalidated_objects` exists because the caller has already
validated every object argument — that is what the flag means, and the pin loop
already honours it. The forwarding barrier did not: it called
`load_and_forward`, which re-walks the membership structure it was just told it
could trust. `load_and_forward_validated` is the existing twin for exactly this
contract, and the profile prices the difference at ~3.7 ns per object argument
(`class_id_of` 3.6-5.0 vs `class_id_of_validated` 1.0-1.3 is the same walk, by
the same margin).

### 4. The descriptor is decoded once, onto an entry that is already fetched

`NativeSiteCache` — the per-call-site entry the dispatch already probes for
0.8 ns — now carries the decoded parameter tags and the return tag. The
argument decode reads tags instead of parsing, and `coerce_native_return_typed`
takes the tag instead of re-scanning for `)`.

**What was tried first and thrown away:** a separate `JitSiteKey`-keyed memo
holding the same shape, so `forward_jit_reference_args` could use it too. It
measured **14-15 ns per lookup** against 0.8-1.1 ns for `NATIVE_SITE_CACHE` in
the same run — more than the parsing it removed. So the decode rides on an
entry that is already being fetched, and `forward_jit_reference_args` was left
exactly as it was. The rung that priced the rejected design is worth more than
the design was.

## Result

### The natives that actually traverse the funnel

`probes/NativeBoundaryProbe.java` (added with this change) times natives by ARGUMENT
SHAPE, which is what the boundary cost scales with. Interleaved A/B, arm order
flipped on alternate pairs, 7 pairs, pinned, last pass of each run:

| rung | base | trial | delta |
|---|---|---|---|
| `System.identityHashCode` (static, 1 object arg) | 175.0 ns | 156.3 ns | **−10.7 %** |
| `StringBuilder.append(int)` (receiver + int) | 861.3 ns | 807.5 ns | **−6.2 %** |
| `AtomicInteger.get` | 1.3 ns | 2.5 ns | — |
| `AtomicInteger.compareAndSet` | 8.8 ns | 10.0 ns | — |
| `String.length` | 2.5 ns | 2.5 ns | ±0 |

The last three are served by the LEAF path or a direct bind and never enter the
funnel, so they should not move — and at 1-10 ns/op they are also at the
millisecond timer's resolution, which is why no delta is claimed for them.
They are in the table as the control: a change that moved them would be
changing something other than the funnel.

### The `String/Regex` row

Three independent series, n = 1 000 000, checksum-verified on every sample
(`500000500000`, zero mismatches across 102 runs):

| series | estimator | builder loop | regex loop |
|---|---|---|---|
| 15 pairs | ratio of medians | −4.1 % | −3.1 % |
| 15 pairs | ratio of medians | −9.8 % | −9.9 % |
| 21 pairs | ratio of medians | −20.4 % | −14.3 % |
| 21 pairs | **median of per-pair ratios** | **−10.2 %** | **−4.9 %** |

The paired estimator is the one to read, and it is the reason the series above
it disagree so widely: a pair runs both arms back to back, so its ratio cancels
whatever the host load was during that pair, while a ratio of medians across a
whole series only works if load is stationary — and on this box it is not
(1-minute load average moved between 26 and 155 during these runs). Trial won
13 of 21 pairs on the builder loop and 12 of 21 on the regex loop, which is
weak on its own; it is the agreement between four estimators, the
native-dense probe and the microbenchmark arithmetic that carries the claim,
not any one of them.

**The arithmetic checks out.** `StringRegexOnly` makes five site-cached native
dispatches per element of its input. At ~14 ns saved per dispatch — 7-9 ns of
counters, ~3 ns of scratch fill, ~3.7 ns of the re-walked forwarding barrier,
~2 ns of descriptor parsing — that is ~70 ns per element against a per-element
cost of roughly 1.5 µs. A few per cent is exactly what the components predict,
and it is what three series measured.

## What is left, and what it costs

* **`record_invocation`, 6 ns.** The registry's per-slot `fetch_add`, the third
  of the three counters. It is in `native-api`, its counts feed the
  `--dump-native-registry` census that CI gates on, and giving it the same
  per-thread treatment needs a per-registry block whose slot count grows with
  lazy registration. Measured and left alone rather than guessed at.
* **`NativeStateSpan`, ~4.5 ns.** Already down from 10.8-14.4 ns as three
  thread-local accesses; what remains is one TLS access and the STW census
  genuinely needs the state.
* **`load_and_forward` on the un-prevalidated path, ~5 ns per object
  argument.** The interpreter's callers have not validated, so the walk is real
  work there.
* **`forward_jit_reference_args` on object-argument sites, ~29 ns.** The
  descriptor walk is most of it and a per-site cache cannot pay for itself at
  14 ns a lookup. Precomputing the reference mask into `JitInvokeInfo` at
  compile time would remove it outright — that struct has 61 construction
  sites, which is a change of a different size.

## Gates

- `probes/SrParity.java` (58 lines) and `apps/probes/StringBuilderShadowSweep.java`
  (747 lines) byte-identical to HotSpot on both arms, and identical between
  arms. `StringRegexOnly` checksums correct at n = 100 000 and n = 1 000 000.
- `NativeBoundaryProbe` checksums identical on HotSpot and both CratonVM arms.
- **Counter exactness**, `jit_counter_block_tests`: four threads each make
  10 000 increments, every one survives its thread's exit and is summed by the
  reader; an out-of-range site index is ignored rather than landing on a
  neighbouring counter; the per-thread counter array stays index-parallel with
  the site-name array.
- `cargo test -p cratonvm-vm --lib`: 2 635 passed, 0 failed.
- The end-to-end `CRATONVM_INTRINSIC_STATS=1` dispatch count varies in
  2 000-unit steps on BOTH arms (base measured 990 003 and 996 003 on
  different runs; trial 994 003 and 996 003), which is tier-up timing deciding
  how many calls reach the compiled path — not lost counts. That is what the
  exactness test above exists to separate, and why the end-to-end count is not
  used as the gate.
