# Compiled code is 20-350x slower than HotSpot on small-method-heavy paths: native crossings ~190 ns, tiny methods called through the generic dispatch (TestMethodPerformance cannot finish)

| | |
|---|---|
| **Status** | **PARTLY FIXED, rest OPEN**; the `MessageBytes` loop was also being banned into the interpreter, fixed 2026-10-13 (see the last section, [doc 55](../../internal/fixed-suite-bugs/tomcat/55-callee-trap-in-a-receiver-keyed-body-was-judged-against-the-declaring-keys-body-charged-as-osrexit-and-decode-banned-FIXED-20261013.md)). Measured 2026-10-05 on `cratonvm-tomcatjitonly-g1-13..17.exe` (`--jdk-only`, JIT on, Windows 11). The `ThreadLocal.get` part (2 us -> 0.12 us) is fixed: [doc 50](../../internal/fixed-suite-bugs/tomcat/50-threadlocal-real-fast-path-never-served-inheritablethreadlocal-unloaded-FIXED-20261005.md). Not a regression and not caused by docs 42-49. |
| **Affected** | `org.apache.tomcat.util.http.TestMethodPerformance` (1.2 billion loop iterations; HotSpot 21 s, CratonVM > 1800 s on all three collectors: `HANG` in the 2026-10-05 three-GC run). Slow but passing under the 1800 s cap: `TestCharsetCachePerformance` (597 s vs 27 s), `TestHttp2Section_8_2`, `TestGenerator` (245 s vs 19 s), `TestELParserPerformance` (31 s vs 4 s). |
| **Probes / tool** | `MicroOps`, `MicroTL*`, `DecParts`, `DecLoop`, `MethodPerfProbe` (not committed; each a short `System.nanoTime` loop). The sampler used for the last section is committed: [`tools/psample`](../../../tools/psample/README.md). |

## Symptom

`TestMethodPerformance` runs `mb.setBytes(IN, 0, 3); mb.toStringType();` and `Method.bytesToString(IN, 0, 3)` for 6 x 100,000,000 iterations each and asserts nothing; the suite counts it by wall time.
Per iteration, JIT on: `MessageBytes` loop **median 6.1 us before doc 50, 4.5 us after** (5 interleaved runs; individual runs range 3.8-10.5 us) (interpreter only 36-47 us; HotSpot 28-50 ns); `Method.bytesToString` loop 166-260 ns (HotSpot 2-4.5 ns). The cap allows ~1.5 us per iteration of the 600 M `MessageBytes` iterations; at 4.5 us the class needs ~2,700 s.
The JIT is working (C2 compiles on the path, `deopts=0`, `c2_bailouts=0`), it is just slow per operation.

## Where the time goes

Single-operation timings, ns per op (CratonVM g1-16 / HotSpot):

| operation | CratonVM | HotSpot |
|---|---|---|
| `new Object` / `new byte[3]` | 27 / 33 | 13 / 10 |
| `Thread.currentThread()` | 33 | 9.6 |
| `WeakReference.get()` / `.refersTo(x)` | 230 / 159 | 10 / 0.4 |
| `Object.hashCode()` / `System.identityHashCode(x)` | 287 / 304 | 10.6 / 9.9 |
| `System.currentTimeMillis()` / `nanoTime()` | 256 / 144-250 | 10.8 / 34-44 |
| `ThreadLocal.get()` (doc 50) | **121-146** (was 1,600-2,000) | 8.4-11 |
| `ByteBuffer.wrap(b,0,3)` / `CharBuffer.allocate(3)` | 277 / 194 | 24 / 33 |
| `new String(byte[],0,3,ISO_8859_1)` | 131-181 | 13-40 |
| `CharsetDecoder.decode(bb, cb, true)`, buffers reused | **2,533** | **7.2** |
| `CharsetDecoder.decode(bb, cb, false)`, buffers reused | **5,749** | 8.6 |
| `Charset.decode(ByteBuffer)` | 2,597 (was 4,028) | 31 |

Three separate costs:

1. **A call from compiled code into a native is ~190 ns end to end**, of which ~90 ns is `safe_native_call_leaf` and the native, ~50 ns the per-site cache probe (`NATIVE_SITE_CACHE` is a thread-local `HashMap` keyed per call), ~30 ns `forward_jit_reference_args`, the rest `contain`/`note_jit_boundary`/SATB flush/census. Cycle checkpoints along `jit_invoke_dispatch` (temporary `CRATONVM_DBG_DISP_CP`, build 15): no single step above ~50 ns after subtracting the 45-cycle `rdtsc` cost of each checkpoint. It is the 100 ns design target (`jit_native_dispatch_profile`) doubled, not a bug. `Object.hashCode`, `identityHashCode`, `Reference.get/refersTo`, the clocks and `Thread.currentThread` pay it; HotSpot inlines them.
2. **Tiny Java methods are still reached through `jit_invoke_dispatch`/`jit_invoke_virtual_mic` from compiled code** (`MIC hit_noentry=3.1M`, `cyc_invoke` 85% of `cyc_mic_total` in `DecLoop`): `Buffer.remaining`, `CharBuffer.flip`, `CharsetDecoder.reset/flush/averageCharsPerByte`, `String.<init>(char[],int,int,Void)`, `HeapByteBuffer.position` were each a dispatch per call in the traced `MessageBytes` loop (~10 per iteration). `inline-resolve REFUSED ... athrow` / `arraylength` is logged for `Buffer.<init>` and `HeapByteBuffer.<init>`, and `Buffer.<init>` is admitted only to the single-pass backend ("it has a String access intrinsic here and the IR tier has none"), 18 KB of machine code for a constructor.
3. **`CharsetDecoder.decode` itself: 2.5 us of a call that makes ONE dispatch (`inflate`, ~250 ns).** The RIP sampler (`tools/psample`, 40 s on `DecLoop`, JIT symbols from `CRATONVM_JIT_PERF_MAP=1`) puts the running thread's samples in many `[c1]`-tier bodies (`ISO_8859_1$Decoder.decodeArrayLoop`, `HeapByteBuffer.position`, `Buffer.position`, `ByteBuffer.arrayOffset`, `CharBuffer.array`) interleaved with `try_jit_site_cached_native_dispatch`, `invoke_on_class_shared_inner`/`invoke_or_native_impl_walk` and `ZgcRealHeap::is_object_address`: C1 code calling C1 code through helpers, plus a by-name resolution path (`FxHasher` over `(Arc<str>, Arc<str>, Arc<str>)`, `force_native_over_real_jdk_bytecode_memoized`) that should not be in a steady-state loop. Not drilled further.

`CRATONVM_PROFILE_SAMPLE_MS` cannot see any of this (98 samples in a 13 s run; compiled loops hold no safepoint poll), and `--no-jit` is slower, so it is cost per call, not a deopt loop or stuck interpretation.

## Not done

* Inline `Object.hashCode`/`identityHashCode`/`Reference.get`/`refersTo`/`Thread.currentThread`/clocks as IR intrinsics (`jit/src/intrinsic_catalogue.rs`; `ea_ir_bridge.rs` special-cases the identity hash but refuses `Object.hashCode` deliberately).
* Find why `HeapByteBuffer.position`, `Buffer.remaining` etc. are not spliced into their callers at C2 in `Charset.decode`, and why the by-name `invoke_on_class_shared_inner` path is still taken per call.
* Give `Buffer.<init>` the IR backend (which String access pins it to the single-pass backend was not identified; the only string work in the constructor is on cold throw arms).
All GC-sensitive JIT changes needing their own validation round; none was attempted here.

## What would close it

`MethodPerfProbe` `MessageBytes` loop <= ~1.5 us/iter (3x from here) so `TestMethodPerformance` fits the suite's per-class cap. `MicroOps` `decode(bb,cb,true)` is the single best indicator (2,533 ns -> < 300 ns).

## 2026-10-06: the same crossing on the monitor and park natives (Linux)

From `docs/internal/performance/r16-monitor-wait-notify-handovers-are-10-to-150x-hotspot-FIXED-20261006.md`
(fat-LTO build of `monitor-wait-notify-perf-1006`, loaded host), `R16MonitorBlockCost`, ns per op,
CratonVM JIT / `--nojit` / HotSpot: `Object.notify()` on an inflated monitor with no waiter
180-360 / 670-1 220 / 30; `notifyAll()` 190 / 710-2 400 / 31; `LockSupport.unpark(self); park()`
(the permit is always there, so `park` returns at once since that branch) 790-1 580 / 1 950-4 120 /
87-90 (two campaigns). In
the last one `park` itself is ~2% of the samples; the rest is this page's crossing
(`try_jit_site_cached_native_dispatch`, `safe_native_call_impl`, `forward_jit_reference_args`,
`decode_dispatch_values_into_shaped`, `jit_invoke_virtual_mic_body`) plus `unpark`'s two lookups of
the target thread (`resolve_thread_id_from_thread_obj` for the virtual-thread test, then
`SharedVm::find_park_state_for_thread_obj`).

## Update 2026-10-06: unchanged on the `dev` tip

`MethodPerfProbe` (the `MessageBytes` loop and `Method.bytesToString`, 300,000 iterations x 5) on `cratonvm-tomcatjitonly-g1-27.exe` (`origin/dev` of 2026-10-06 merged with docs 42-52): **4.1-5.0 us** and 192-259 ns per iteration, against 4.2-4.7 us / 195-260 ns on `g1-22` and 28-34 ns / 2 ns on HotSpot. The waves that landed on `dev` meanwhile (SBR round 4, r16 matrix, loop-carried EA) did not move this loop. `TestMethodPerformance` still `HANG`s at 1800 s on all three collectors (three-GC run of 2026-10-06).

## Update 2026-10-13: the `MessageBytes` loop was being banned into the interpreter, fixed (doc 55); the crossings below are untouched

[Doc 55](../../internal/fixed-suite-bugs/tomcat/55-callee-trap-in-a-receiver-keyed-body-was-judged-against-the-declaring-keys-body-charged-as-osrexit-and-decode-banned-FIXED-20261013.md).
In about three runs of four the loop above was not slow because of the crossings: `CharsetDecoder.decode(BB,CB,Z)` was
**interpreted on 1.41 M of 1.5 M iterations** (`CRATONVM_DBG_INTERP_FRAMES=1`). A trap in the body published under the
receiver class's key (`ISO_8859_1$Decoder.decode`) was judged against the declaring key's body, charged as `OsrExit`
and, two charges later, `MakeNotCompilable` (`jit_skip_set`, `entry_ineligible`). The "3-10 us, individual runs range
3.8-10.5" spread of the status table is that bistable outcome, not noise. The 2.5 us of section 3 is **not** that
ban: `MicroOps` `decoder.decode(bb,cb,true)`, re-run on the loaded host, is 1.27-1.60 us on the `dev` tip and on the
fix alike (4 runs), so section 3 stands as a cost per call of ~1.3-1.6 us now, not a missed compile.

`MethodPerfProbe` / `MbLoop2` on a **loaded** host (other sessions were building), `dev` tip (`g2-1`) against the fix
(`g2-11`), 6 interleaved rounds: `MessageBytes` **10.0-13.7 us -> 2.0-3.9 us** per iteration, in 6 of 6 rounds;
`Method.bytesToString` 208-540 ns on both (unchanged). The rest of this page stands: the native crossings, the
tiny-method dispatches and `Method.bytesToString` are the remaining gap to HotSpot's 28-50 ns / 2-4.5 ns, and the loop
now sits at 1.3-2.6x the ~1.5 us the class needs, not 3-9x. `TestMethodPerformance` still did not finish in 1,700 s on
either build (two runs each), so whether it fits the suite cap is open; re-time it on an idle host first.

Left open for this page: the `Buffer.<init>` / `HeapByteBuffer.<init>` dispatches; the by-name resolution path in
`DecLoop`'s samples.

On the merged `dev` tip of 2026-10-13 (`g2-12`, 5 rounds, same host): chunks of 250,000 iterations run **1.4-2.4 us** at first and drift to **1.8-5.0 us** after ~1 M iterations (baseline 8-18 us); `MethodPerfProbe` `MessageBytes` 1.4-3.7 us. The drift is not explained (doc 55, "Not done").
