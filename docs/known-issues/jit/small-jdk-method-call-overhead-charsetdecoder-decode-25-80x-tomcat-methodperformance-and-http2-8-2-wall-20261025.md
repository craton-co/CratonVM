# Allocation-heavy loops over small JDK methods run 15-80x slower than HotSpot: `CharsetDecoder.decode` 200-460 ns against 8 ns; `TestMethodPerformance` takes 10 minutes against 16.5 s, `TestHttp2Section_8_2` 450-700 s against 147 s

> **j27 wave 8 (lane W8UPC, 2026-10-28): NOT FIXED, REVIEWED, PENDING HOST VERIFICATION. No code changed.** Re-read wave 5's `CRATONVM_JIT_IR_SPLICE_TAIL_HANDLER` (opt-in) end to end: `jit/src/ir.rs` `ir_splice_tail_handler_enabled` is `runtime_flag_on` (the DECLOOP table's `on` column came from a build where it was default ON; the tree is opt-in, as its doc says), `splice_handler_run_end_admitted` admits a run ending at `code_len` only under the switch, and the two halves of `splice_tail_handler_follows_return` agree: the VM resolver (`jit_bridge.rs`, `trailing_masked_handler`, handler pcs `row.2`, callee-relative) and the builder's multi-return pre-scan (`IrBuilder::build`, `row.handler_pc - base`) ask the same predicate over the same masked bytes, so a body the resolver admits is the body the builder closes as multi-return. No defect found; nothing in the remaining list is buildable without a measurement. The lever the data supports is still the covered-nested-body splice ([j27-covered-try-nested-bodies-stay-calls-charsetdecoder-wrap-proposal-20261028](j27-covered-try-nested-bodies-stay-calls-charsetdecoder-wrap-proposal-20261028.md)): a planner change across `ir.rs` and `jit_bridge.rs` with deopt-state consequences, which a code-only lane should not land blind. Host: the DECLOOP recipe below (`J27DecRows`, `J27SmallCallRows`, `J27StrpinRows` facts both modes and `--nojit`; timing interleaved).

> **j27 wave 5 (lane DECLOOP, 2026-10-28): NOT FIXED (diagnosed, measured locally; one opt-in switch, flat).** Item 3 of "Not done" done: the `dec` row taken apart, on Windows (P-cores, builds from this tree).
> **What runs.** (a) In `DecLoop` itself `main` is pinned to the single-pass backend (`[ir] admission DecLoop.main: pinned to the single-pass backend: it has a String access intrinsic here` -- the `sb` row), so every row there runs in an `osr/sp` body: `reset` / `decode` are calls and `osink = ...` goes through `jit_putstatic_object` (+ `record_object_ref_payload_slow`, the SATB barrier): about 15 % of the dec-loop samples. That is a probe artefact shared by all four rows (STRPIN's area). New probe `tools/probes/jit/j27/J27DecRows.java` runs each row in its own method so it compiles like a Tomcat caller. (b) `CharsetDecoder.decode(ByteBuffer)` is `full/ir`, 44 bodies spliced (23 KB). What stays a call (sampler on the `dec3` row = `reset; decode(wrap, held CharBuffer, true); flush`, no `CharBuffer.allocate`): `ISO_8859_1$Decoder.decodeLoop` 24 % (inside `decode(BB,CB,Z)`'s `try { decodeLoop } catch (RuntimeException | Error)`: `ir-splice-handler-covered-nested ... kept as calls`); `ByteBuffer.wrap` 23 % plus its `Buffer.limit/position` setters 18 % (`wrap` is never spliced: `callee-exception-table (handlers: not the try-call/catch-wrap-throw shape)` because its handler is the tail of the body); `System$1.uncheckedInflateBytesToChars` 9 % (`JLA` `invokeinterface`, `ir-splice-target-not-provably-monomorphic`); the `dec3` body itself 19 %. Refused but cold: `decodeBufferLoop` (catch-all), `CharBuffer.put` / second `remaining` (budget spent). Bails: none on the hot path (two `unallocated_value` lines are elision retries of `HashMap.putVal` / `decodeArrayLoop`, which then compile).
> **Code.** `CRATONVM_JIT_IR_SPLICE_TAIL_HANDLER` (**opt-in `=1`**, `jit/src/ir.rs` `ir_splice_tail_handler_enabled`, `splice_handler_run_end_admitted`, `splice_tail_handler_follows_return`; builder multi-return pre-scan in `IrBuilder::build`; VM resolver `trailing_masked_handler` in `vm/src/runtime/interpreter/jit_bridge.rs`): admits the `wrap` handler shape. Measured flat, so it stays off: the planner then refuses `wrap` as `ir-splice-callee-handler-covered-nested-body`, and with `CRATONVM_JIT_SP_BOUND_SITE_IC_CTOR=1` too `wrap` splices but `HeapByteBuffer.<init>` (and the setter chain) stays a call. The real lever is a covered nested body kept spliced: [j27-covered-try-nested-bodies-stay-calls-charsetdecoder-wrap-proposal-20261028](j27-covered-try-nested-bodies-stay-calls-charsetdecoder-wrap-proposal-20261028.md).
> **Numbers** (ns/op, medians of 5 interleaved processes, each row itself a median of 5 reps of 1,000,000; `on` = build default, `off` = `..._TAIL_HANDLER=0`, `ctor` = `..._TAIL_HANDLER` + `CRATONVM_JIT_SP_BOUND_SITE_IC_CTOR=1`; the build these came from had the switch default ON, so `on` = `=1`):
>
> | row | HotSpot 25 | on | off | ctor |
> |---|---|---|---|---|
> | J27DecRows dec | 18.5 | 173.5 | 163.9 | 176.8 |
> | J27DecRows decbb (no wrap) | 14.0 | 116.9 | 110.6 | 130.3 |
> | J27DecRows dec3 (no allocate/flip) | 2.4 | 142.8 | 126.6 | 134.2 |
> | J27DecRows wrap | 6.8 | 41.7 | 40.0 | 41.5 |
> | J27DecRows alloc (`CharBuffer.allocate(3)`) | 9.3 | 33.9 | 32.2 | 33.9 |
> | DecLoop dec (reps 3-4) | 9.6 | 230.9 | 200.8 | 221.6 |
> | DecLoop wrap / pos / sb | 5.8 / 1.8 / 4.8 | 72.7 / 75.4 / 87.3 | 70.6 / 73.6 / 85.0 | 77.8 / 77.1 / 89.7 |
>
> Facts (`J27DecRows`, `J27SmallCallRows`) equal HotSpot 25 in `--jdk-only`, `--compatible` (output present, no fast-boot workaround needed) and `--nojit`, with the switches on and off. Host recipe: `java -cp <dir> J27DecRows` vs `cratonvm [--compatible] --cp <dir> J27DecRows`, diff (expected lines in the probe header); timing `-Dj27.timing=true [-Dj27.only=<row>]`; cause `CRATONVM_DBG_JITC=1 ... -Dj27.only=dec3 J27DecRows 3000000 3 2>jitc.txt; grep -E 'ByteBuffer\.wrap.*(REFUSED|refused)|covered-nested' jitc.txt`.

> **j27 wave 5 (lane STRPIN, 2026-10-28): PARTLY FIXED IN CODE, measured on Windows (local build), PENDING HOST VERIFICATION.**
> (1) **The largest cost found was a lost compile, not a slow call.** A RIP sampler on a chunked setter loop showed the
> caller running its `full/sp` body: every method that splices `Buffer.limit(I)` / `position(I)` had its optimizing
> compile REFUSED (`ir_lower: a spliced guard's required caller chain cannot be published ... a scope holds a value the
> chain sinks cannot rebuild`). Cause: the boolean phi fold creates a new compare that only the cold throw arm's chain
> frame names, and the scheduler left it after the guard. Fixed: new `jit/src/sched_named_before_anchor.rs`,
> `CRATONVM_JIT_IR_SCHED_NAMED_BEFORE_ANCHOR` (default ON, `=0` restores), page
> [j27-fold-created-frame-named-value-scheduled-after-its-guard-refuses-ir-20261028](../../internal/jit/j27-fold-created-frame-named-value-scheduled-after-its-guard-refuses-ir-FIXED-20261029.md).
> Medians (ns/op, 5 interleaved reps, P-cores), base / `=0` / HotSpot: `J27StrpinRows bbsetters` 3.2 / 36.2 / 0.4,
> `setters` 26.4 / 64.9 / 1.2; `J27SmallCallRows chain` 5.7 / 361 / 0.4, `charbuf` 3.7 / 30.1 / 1.1, `setters`
> 27.2 / 38.8 / 0.8; `--compatible` `J27StrpinRows dec` 149 / 2192 / 8.7. Facts unchanged.
> (2) **`CRATONVM_JIT_OSR_GUARDED_SUPER_BIND=1` A/B: no row moved** (DecLoop dec 163 vs 160, pos 59.6 vs 61.4; every
> `J27SmallCallRows` / `J27StrpinRows` row inside the spread). It stays opt-in. Facts identical with it.
> (3) **`DecLoop.main` is pinned to the single-pass tier** by the String-intrinsic pin (its `new StringBuilder(..)`
> row), so its `pos` / `dec` rows measure the single-pass OSR body; `J27StrpinRows pinned-*` prices it (`pinned-setters`
> 62 vs 11.7 with `CRATONVM_JIT_IR_SB_PREFIX=1`). See the pin page's j27 wave 5 block.
> (4) **What is left** (`dec` 131-160 ns, `wrap` 32-55, `setters` 26 against 1-9 on HotSpot): no single hot spot.
> The chunked `settersChunk` IR body takes 99 % of the samples, spread over ~1.6 KB of loop body for four setters (the
> general per-node cost of the frame-homed lowerer). In `dec`: `CharsetDecoder.decode(BB)` 24 %, `ISO_8859_1$Decoder.decodeLoop`
> 21 %, the covariant bridges `ByteBuffer.limit(I)Buffer` / `position(I)Buffer` 8 % + 7 % called from the `wrap` body
> (the `Buffer.<init>` chain, item 1 below), and `ByteBuffer.position(I)ByteBuffer` `full/sp` called from
> `decodeArrayLoop` (its C2 body is refused by the acceptance gate: "evidence: none"). Probe
> `tools/probes/jit/j27/J27StrpinRows.java` (chunked rows; `-Dj27.secs=N` loops for a sampler).

> **j27 wave 1 (lane STRRX, 2026-10-28): NOT FIXED.** Read for a String-side or call-admission cause and found
> none in this lane's area: the `decode` / `wrap` chain holds no String member that bails the planning gate or
> pins (`string_site_bails_ir_planning` / `string_intrinsic_pin_verdict`), and the setter chain behind
> `Buffer.<init>` was already shown not to be the cost (item 1 of "Not done"). Two notes for whoever takes it:
> `CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS` is **default ON** since JIT r24 (`jit/src/ir.rs`), so
> `CharsetDecoder.decode(BB,CB,Z)`'s try/catch-wrap shape can already splice -- the comment above
> `handler_rows` in `vm/src/runtime/interpreter/jit_bridge.rs` still says "default OFF" (doc drift, not this
> lane's file); and the next step is item 3 of "Not done", taking the `dec` row apart the way `wrap` was
> (`tools/probes/DecLoop.java`, `CRATONVM_DBG_JITC=1` for `inline-plan` of `CharsetDecoder.decode`).

> **j27 wave 3 (lane SMALLCALL, 2026-10-28): NOT FIXED, PENDING HOST VERIFICATION.** No speed change landed: this lane cannot build or measure, and the two levers the code offers either were already A/B'd here (the `Buffer.<init>` exact-slot-0 seeding of item 1: no `WMin` change) or are frame switches the project keeps OFF until a frame-bound workload measures them, which this page is. What changed:
> (1) **Diagnosis of the `osr-splice-guarded-refused ... CalleeUnresolved` line** quoted in the second pass. It is not a lookup failure. `plan_osr_door_guarded_splices` (`jit/src/lib.rs`) reports every `None` from the VM's receiver resolver as `CalleeUnresolved`, and that pass hands the resolver no direct binder, so a call-carrying body is refused ("neither spliced nor direct-bound"). `ByteBuffer.position(I)` / `limit(I)` are `{ super.position(p); return this; }`, and `Buffer.position(I)` / `limit(I)` build their exception message through an `invokedynamic` concat (`createPositionException`), so the single-pass OSR door cannot splice any level of the setter chain by design. That explains the probe's OSR'd `main` loop only, not the `full/ir` bodies of the real workload. The line now ends with a hint pointing at the resolver's own `inline-resolve REFUSED ... route=receiver=<id>` line (new `jit/src/osr_guarded_refusal_hint.rs`, debug output only, unit-tested).
> (2) **Frame anatomy of the small bodies.** The 0x230-byte frame quoted in the third pass is mostly fixed reservations, not values: on Win64, 48 bytes reserve and shadow, the 256-byte deopt register image (`ir_deopt_regs_bytes`, whole `SavedRegisters` unless `CRATONVM_JIT_IR_DEOPT_IMAGE_SIZED=1` or the body has no exit that can name a register), one save slot per register of the XMM/GPR files unless `CRATONVM_JIT_IR_SAVE_BANDS_SIZED=1`, 40 bytes bookkeeping, then locals and colours. The fresh-frame clear zeroes most of it per call; the deopt GPR half leaves the clear only under `CRATONVM_JIT_IR_DEOPT_IMAGE_CLAIM=1`. All of these were measured neutral on kernels (r16/r24 triage: "re-measure only on a frame-bound workload"); the setter chain here is that workload, so the next step is the A/B below, not new code. Proposal page: [j27-small-call-frame-arms-and-osr-super-bind-proposal-20261028](j27-small-call-frame-arms-and-osr-super-bind-proposal-20261028.md).
> (3) Comment drift fixed in `resolve_inline_site_from` (`vm/src/runtime/interpreter/jit_bridge.rs`): `CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS` (the `CharsetDecoder.decode(BB,CB,Z)` try/catch splice) is default ON since r24; the comment said OFF.
> **Probe** `tools/probes/jit/j27/J27SmallCallRows.java` (expected HotSpot 25 lines in its header): wrap/position/limit/remaining through `Buffer`, `ByteBuffer`, `CharBuffer`; the five `IllegalArgumentException` / `IndexOutOfBoundsException` / `InvalidMarkException` messages cold and after warm-up; ISO-8859-1 decode with buffer positions; UTF-8 under REPORT (`MalformedInputException Input length = 1`, input position 1) and REPLACE; six warm checksums; `-Dj27.timing=true` prints `time <row> <ns/op>` for wrap, setters, chain, charbuf, iso-decode, sb.
> **Host recipe.** (a) Facts: `java` vs `cratonvm` both modes and `--nojit`, diff. (b) Cause: `CRATONVM_DBG_JITC=1 CRATONVM_DBG=jit-compiled,tierup-decline,ir-bailout cratonvm --cp <dir> -Dj27.timing=true J27SmallCallRows 2>jitc.txt`, then `grep 'ir-frame-anatomy' jitc.txt | grep -E 'java/nio/|J27SmallCallRows'` (frame and clear bytes per small body), `grep 'inline-resolve REFUSED java/nio/' jitc.txt | sort | uniq -c` (why a setter or bridge stays a call, by route), `grep -E 'ir-nested-guarded-hot-cap|osr-splice-guarded-refused' jitc.txt`. (c) Frame arms, interleaved, pinned, 5 reps each, `time` lines plus `perf stat -e instructions:u` at N and 2N (doubling recipe): base; `CRATONVM_JIT_IR_DEOPT_IMAGE_SIZED=1 CRATONVM_JIT_IR_SAVE_BANDS_SIZED=1`; that plus `CRATONVM_JIT_IR_DEOPT_IMAGE_CLAIM=1 CRATONVM_JIT_IR_FRAME_PARAM_LOCALS_ONLY=1 CRATONVM_JIT_IR_FRESH_CLEAR_COVER=1 CRATONVM_JIT_IR_LEAF_FRAME_ELIDE=1`. A row that moves names the frame as the floor (item 2); a flat result moves the hunt to the allocation path (item 4) and the 6.9 KB `wrap` body (`CRATONVM_DBG_JIT_DISASM`). Facts must stay identical in every arm.

| | |
|---|---|
| **Status** | **OPEN, performance**, filed 2026-10-25, narrowed the same day (one stuck-interpreted defect fixed, see the update below). Residual of the "slow host edge timeouts" group of the 35-diff census ([doc 63](../../internal/fixed-suite-bugs/tomcat/63-tomcat-35-diff-census-linux-host-closed-FIXED-20261025.md)); the correctness and stall defects of that census are fixed, these two classes still run past the suite's 600 s cap on a 4-vCPU host. |
| **HotSpot** | `TestMethodPerformance` 16.5 s (Windows), 37.8 s (the census's 4-vCPU host); `TestHttp2Section_8_2` 147 s / 300 s |
| **CratonVM** (Windows, 2026-10-25, JIT on, `CRATONVM_JIT_CALLEE_DOOR_DEFERS_TO_WORKER` on) | `TestMethodPerformance` passes in about 10 minutes (815 s with the switch off); `TestHttp2Section_8_2` passes, 6658 of 6658, in 446 s (an earlier run) to 691 s (while a cargo build ran) |

## What the loops are

`TestMethodPerformance` is 6 x 100,000,000 iterations of `mb.setBytes(in, 0, 3); mb.toStringType();` (the MessageBytes -> `ByteChunk.toString` -> `StringCache` -> `CharsetDecoder.decode` path) and then of `Method.bytesToString`. Measured per iteration, same machine, three loops of 1-4 million iterations after warm-up:

| Loop | HotSpot 25 | CratonVM |
|---|---|---|
| `mb.setBytes; mb.toStringType()` | 27-30 ns | **500-650 ns** (Windows), 550 ns (Linux host) |
| `dec.reset(); dec.decode(ByteBuffer.wrap(in, 0, 3))` (ISO-8859-1) | 8 ns | **200-460 ns** |
| `new StringBuilder("abc").append(1).toString()` | 4 ns | 63 ns |
| `Method.bytesToString(in, 0, 3)` (no allocation; the test's second loop) | 1.7 ns (168 ms per 100 M) | not measured |

## What was found

* It is not interpretation. `CRATONVM_DBG=interp-frames` counts 110,036 interpreted frames over 300,000 iterations of the first loop (0.4 per iteration); the compiled bodies are `full/ir` and `full/sp` tier ones (`CRATONVM_DBG_JIT_DISASM`).
* It is not one hot spot. An all-thread stack sampler (Windows, `StackWalk64` + the pdb; 90 samples of the loop thread) put 70 of 90 samples in compiled code and mapped them onto the disassembly headers: `CharsetDecoder.decode` 19, `ISO_8859_1$Decoder.decodeLoop` 7, `ByteBuffer.limit` 6, `ByteBuffer.wrap` 5, `CharBuffer.limit` 5, `ByteBuffer.position(I)` (`full/sp`) 5, `CharsetDecoder.implFlush` 4, `CharBuffer.position` 4, `ByteBuffer.position(I)Buffer` 4, `Buffer.position` 3 ... and 6 in `carve_tlab_chunk_locked` (TLAB zeroing). Tiny setters (`limit`, `position`) take 5-6 % each, the shape of calls that are not inlined.
* Allocation is part of it: with the GC statistics line (`CRATONVM_GC_STATS=1 --verbose:gc`) a 10,000-datagram UDP run zeroes 4 GB of TLABs (27,348 refills of about 148 KB) while each receiver thread allocated 8 MB (`ThreadMXBean.getThreadAllocatedBytes`); that is consistent with a TLAB retired at every blocking native call and a fresh one taken by the next allocation, which was not confirmed. `new Object()` stored to a static costs 32 ns on the Linux host against 6 ns.

## Update 2026-10-25 (second pass): one defect found and fixed, the generic gap remains

A loop over `CharBuffer.position` / `limit` ran at 2,400-6,300 ns/op for the rest of the process after the first rep because the methods ended `compile-failed` (an abandoned C2 supersede was charged to `tier_fail_count`; a callee deopt then withdrew the baseline). Fixed: [doc 65](../../internal/fixed-suite-bugs/tomcat/65-abandoned-c2-supersede-spent-the-retry-budget-then-withdrawn-callers-stayed-interpreted-FIXED-20261025.md). `tools/probes/DecLoop.java` after the fix (Windows, pinned to the P-cores, 1,000,000 iterations per rep, reps 1-5):

| Row | HotSpot 25 | CratonVM |
|---|---|---|
| `dec.reset(); dec.decode(ByteBuffer.wrap(in, 0, 3))` | 9-10 ns | **190-250 ns** |
| `ByteBuffer.wrap(in, 0, 3)` stored to a static | 5 ns | **58-70 ns** |
| `held.position/limit` x2 on two buffers + `remaining()` | 1.4-2 ns | **63-94 ns** (about 18 ns per call) |
| `new StringBuilder("abc").append(i).toString()` | 4.6-5.4 ns | **77-120 ns** |

What this says: the gap is uniform and does not depend on the `CharsetDecoder` path. The setter row has no allocation and still costs about 18 ns per call, which points at small calls that are not inlined into the caller (not confirmed by a disassembly; the OSR-compile log shows `main` splicing `CharBuffer.position` / `limit` and then `osr-splice-guarded-refused ... CalleeUnresolved` for `CharBuffer.position` / `limit`). The allocating rows add about 50-60 ns per object (`wrap`: one `HeapByteBuffer`), against 5 ns for a bump allocation in HotSpot. The samples in `carve_tlab_chunk_locked` named earlier are the zeroing of fresh TLABs.

## Update 2026-10-25 (third pass): where the `ByteBuffer.wrap` row goes

Isolated with `tools/probes/CallAllocMicro.java` (classes loaded first): `new Object()` 6 ns, `new int[3]` 8-9 ns, `new P(i, 2)` 6.5-9 ns, static call 0.7 ns, virtual call 3.3 ns (HotSpot 2.6-6 / 4 / 3.7 / 0.4 / 0), so plain allocation and a constructor are close to HotSpot once the loop body is spliced. `ByteBuffer.wrap(in, 0, 3)` is 53-130 ns (HotSpot 5). The sampler with the new `rbp`-chain walk (`tools/win-stack-sampler`, ` # ret ...` suffix) names the callers on a loop that only calls `wrap`:

* `ByteBuffer.wrap` (`full/ir`, a 6.9 KB body for a few lines of Java) 53 % of the loop thread's samples, spread over the whole body (no hot instruction);
* `ByteBuffer.limit(I)Buffer` 18 % and `ByteBuffer.position(I)Buffer` 15 %, **called from the `wrap` body**: `Buffer.<init>` runs `limit(lim); position(pos)`, which are spliced (`Buffer.<init>` is inlined into `ByteBuffer.<init>` and on into `wrap`) but the setters behind them are left as real calls (`ir-nested-guarded-hot-cap` plans them inside `ByteBuffer.<init>` only). Each of those IR bodies opens with a 0x230-byte frame cleared by about a dozen 32-byte stores before it does anything (`ByteBuffer.limit(I)Buffer`: `sub rsp,230h`, `vmovdqu [rbp-..],ymm5` x12), and each reaches `Buffer.limit` through two more covariant-bridge levels.

So one `wrap` is roughly a dozen calls (`wrap`, `HeapByteBuffer.<init>`, `ByteBuffer.<init>`, `Buffer.<init>`, then `limit` and `position` through three bridge levels each) where HotSpot has none. At about 5 ns per call that is the measured 55-60 ns. Switches tried on the `wrap` loop (`CRATONVM_JIT_IR_INLINE_BUDGET_WIDE`, `..._IR_NESTED_GUARDED_HOT_CAP_LEAF`, `..._IR_SPLICE_LEAF_PAST_DEPTH`, `..._IR_RECURSIVE_INLINE`): best-of-batch 32-61 ns in the base runs and 32-43 ns with them, inside the run-to-run noise of this machine (other sessions were building), so no effect is claimed.

A separate and much larger effect, found on the way: a class first used after the loop's OSR compile (`new P(i, 2)`: 320 ns against 3.5 ns), now fixed: [doc 66](../../internal/fixed-suite-bugs/tomcat/66-osr-body-kept-deferred-new-and-dispatch-helper-for-a-class-first-used-after-the-compile-320-ns-FIXED-20261025.md).

## Not done

Open, in order of what the data supports: (1) the setter chain behind `Buffer.<init>` stays a guarded call in `wrap` because its nested row is resolved for the constant-pool class (`Buffer`, id 247) while the receiver is the `new HeapByteBuffer` (id 419), so the exact-receiver proof never holds; seeding slot 0 of the constructor chain with the class of the `new` before `invokespecial <init>` (a 30-line planner change, tried and reverted) splices all four bodies (`ByteBuffer.limit`/`position` and the `Buffer` setters, 324 bytes) with `wrap` still compiled by C2, and `WMin` stays at 15-17 ns with and without it (interleaved A/B, pinned P-cores), so the setters are not what separates `wrap` from HotSpot's 4 ns - the floor is the allocation plus the frame the C2 body clears (item 2); (2) the 0x230-byte frame that every small IR body clears in its prologue; (3) the CharsetDecoder row (`dec`, 190-250 ns) has not been taken apart the way `wrap` was: its remaining ~130 ns beyond `wrap` is `reset()`, `decode(ByteBuffer, CharBuffer, boolean)`, `flush`, `implFlush`, `flip` and a `CharBuffer.allocate`, all calls of the same shape; (4) the TLAB-retire-per-blocking-region and escape-analysis candidates of the first pass remain unmeasured. `TestMethodPerformance` and `TestHttp2Section_8_2` were not re-run and stay on the suite's slow list; neither fails on content.
