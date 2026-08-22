# `HttpContentDecompressorTest` — the step this page prescribed is landed and measured; what is left is not this class's

**Status: RETIRED 2026-08-20 as a page.** Every test in the class passes and
always did; the class's wall was one parameterisation (`snappy`); the
prescribed fix for it has landed, and the residual is now decomposed,
quantified and re-homed. Nothing about this class is unknown any more, and
nothing that remains is specific to it.

Superseded page: `known-issues/netty/httpcontentdecompressortest-hang-20260816.md`
(itself a re-diagnosis of the 2026-08-17 `ByteBuffer`-accessor page).

| | predecessor said | now |
|---|---|---|
| the class | 8/8 SUCCESSFUL, 642 s, `HANG=1` at the suite cap | 8/8 SUCCESSFUL, **539 s**, still over the cap |
| the wall | 88 % is `testZipBomb` #5 `snappy` | 86 % is the same test, **462 s** (was 566 s) |
| the next step | "a thin `*_DIRECT_FN` bind for `VarHandle.get` … worth roughly a quarter of this class" | **landed**, measured **1.18x** on the phase and **1.23x** on the test |
| the residual | "compiled-Java throughput on a byte-shuffling loop" | **63 % is VM runtime, 32 % is compiled code** — decomposed below, each part re-homed |
| the blast radius | not asked | 274-class slice, one binary, one class differs — and it goes **FAIL -> PASS** |

Measured 2026-08-20 on Azure host 2 (Linux x86_64, 8 cores, JDK 25), release
build, **one binary** (`bin/cratonvm-vhread20260820`, `md5 c1320f36…`) with the
arms selected by a kill switch, load average recorded beside every number.
Cross-checked against HotSpot 25 on the same host and classpath.

---

## 1. The fix: `VarHandle` read modes are off the funnel

The predecessor page's census is what named this, and it named it precisely:

| invocations per 4 MiB | native |
|---:|---|
| **21 368 822** | `java/lang/invoke/VarHandle.get([Ljava/lang/Object;)Ljava/lang/Object;` |
| 4 261 826 | `java/nio/DirectByteBuffer.get(I)B` |
| 523 872 | `VarHandle.set` |
| 197 536 | `DirectByteBuffer.put(IB)` |
| 132 791 | `java/lang/Enum.ordinal()I` |
| **26 673 142** | total |

~5.1 `VarHandle.get` per output byte, because netty 4.2 checks the reference
count on every buffer accessor and `RefCnt.VarHandleRefCnt.isLiveNonVolatile`
is `(int) VH.get(instance)` on an ordinary `int` instance field.

A VM-side fast path for exactly that read already existed
(`try_varhandle_instance_field_read`) and sat INSIDE `jit_invoke_dispatch`, so
every call paid the SATB flush, the reference-argument forwarding, the site-key
revalidation and two thread-local map probes before reaching it.
`perf/varhandle-read-direct-bind-20260820` binds the four read modes to thin
`*_DIRECT_FN` helpers instead, at the two doors an `invokevirtual` can reach
(the single-pass ladder and the OSR builder — the IR door is gated
`is_static || is_special`, which is why `Integer.intValue` is single-pass-only
too). Reference returns are deliberately left on the funnel; see the cell's own
doc for why (`unbox_poly_return_checked`'s W6-1 rule reads the call site's
declared class, which a baked direct call cannot carry).

### The rung

`probes/VarHandleReadRate.java`, same binary, arms selected by
`CRATONVM_JIT_VARHANDLE_READ_DIRECT_HELPERS`:

| rung | HotSpot 25 | bind OFF | bind ON | ratio |
|---|---:|---:|---:|---:|
| `VH.get` int | 0.02 ns | 115.10 | **58.88** | 1.95x |
| `VH.getAcquire` int | 0.21 ns | 103.90 | **58.29** | 1.78x |
| `VH.get` long | 0.02 ns | 110.02 | **59.40** | 1.85x |
| netty `refCnt` shape (get + getAcquire) | 0.31 ns | 206.42 | **127.18** | 1.62x |
| *control* plain field read | 0.23 ns | 2.08 | 2.37 | flat |
| *control* empty loop | 0.30 ns | 1.05 | 1.09 | flat |
| *control* `VH.get` array element | 0.05 ns | 423.62 | 436.85 | flat |

The array-element control is the one that says the bind stayed inside its
scope: an array-element handle carries two coordinates, so the descriptor
filter refuses it and it keeps the generic dispatch in BOTH arms.

### The phase

`NettyZipBombPhases snappy 8`, G1, interleaved, two rounds, host load 2.58–2.88:

| phase | OFF r1 | ON r1 | OFF r2 | ON r2 | HotSpot |
|---|---:|---:|---:|---:|---:|
| compress | 6 861 | 6 626 | 6 884 | 6 449 | 161 |
| decompress | 11 952 | **9 283** | 11 649 | **9 095** | 105 |
| total | 19 302 | **16 388** | 19 026 | **16 012** | 604 |

**1.18x on the phase, 1.28x on the decompress half**, in both rounds.

**And it survives a repeat in a different window**, which is the test the
predecessor page's own 1.45x claim failed. Same script, host load 5.15–6.85 —
more than double the first window's — and the ratio does not move:

| phase | OFF r1 | ON r1 | OFF r2 | ON r2 | HotSpot |
|---|---:|---:|---:|---:|---:|
| compress | 8 195 | 7 420 | 7 807 | 7 411 | 177 |
| decompress | 13 237 | **10 652** | 13 117 | **10 406** | 117 |
| total | 22 131 | **18 684** | 21 446 | **18 537** | 741 |

1.18x / 1.16x on the phase and 1.24x / 1.26x on decompress, against 1.18x and
1.28x at the lower load. Both arms move with the host together; the ratio does
not.

### The engagement, before any number was believed

`--dump-native-registry` on `snappy 4` with the bind on:

| | OFF | ON |
|---|---:|---:|
| `VarHandle.get` family through the funnel | 21 368 822 | **2 161 914** |
| all natives through the funnel | 26 673 142 | **7 466 279** |

and `CRATONVM_DBG=intrinsic-stats` reports `served=9 603 208 declined=0` for
`snappy 2`, with the funnel's own VarHandle fast path down to 87 hits. The
bind-site line reports `VarHandle.read=2/0` on that workload (two sites, both
at the single-pass door) and `16/0` on `probes/VarHandleReadOracle`, which is
what says both doors are live and which one fires depends on the shape.

### The correctness pin

`probes/VarHandleReadOracle.java` reads every primitive kind through all four
read modes at values a truncation or sign-extension bug cannot survive
(`Long.MIN_VALUE`, a NaN payload, `char` 0xFFFF, `byte` -1), plus the shapes the
bind must refuse (static-field, array-element and `ByteBuffer`-view handles),
plus the `WrongMethodTypeException` rule and a null coordinate. On the Azure
host, real-JDK mode: **CratonVM bind-ON and bind-OFF are byte-for-byte
identical**, and both match HotSpot on every line **except one**, which is
pre-existing and unrelated — see §4.

---

## 2. The class, now

`probes/PerTestProgressRunner.java`, real-JDK mode, G1,
`-Djunit.jupiter.execution.timeout.mode=disabled`, one binary, both arms:

| test | 2026-08-18 page (load 3.9–6.5) | **bind ON** (load 2.0–4.0) | bind OFF (load 4.0–20.2) | HotSpot 25 |
|---|---:|---:|---:|---:|
| `testZipBomb` #1 `gzip` | 18 739 | 17 254 | 27 083 | 5 018 |
| `testZipBomb` #2 `deflate` | 16 578 | 16 923 | 22 923 | 5 405 |
| `testZipBomb` #3 `br` | 18 535 | 18 999 | 19 597 | 4 519 |
| `testZipBomb` #4 `zstd` | 18 022 | 18 097 | 19 366 | 1 683 |
| **`testZipBomb` #5 `snappy`** | **566 348** | **461 766** | 709 775 | 12 077 |
| `testBrotliDecodingHonorsMaxAllocationAsOutputCap` | 3 531 | 4 960 | 7 743 | 1 775 |
| `testInvokeReadWhenNotProduceMessage` | 6 | 9 | 10 | 490 |
| `testFlowControlHandlerEmitsOneMessagePerRead` | 5 | 8 | 9 | 629 |
| **class total** | **642 322** | **539 000** | 807 000 | **15 000** |

All eight are `SUCCESSFUL` in every CratonVM arm. The four non-snappy
`testZipBomb` parameterisations do not move between the ON arm and the
predecessor page's numbers, which is the expected shape: their kernels are this
VM's zlib/brotli/zstd natives, not netty's own Java.

**Do not read the OFF column as this bind's A/B.** The host load ran from 4.0 to
**20.22** during that arm — other sessions started two rustc builds and a second
VM run on the shared box mid-way — so it overstates the effect by an unknown
amount, and a 13-minute class run cannot be re-taken often enough to wait the
host out. It is printed because deleting an arm that ran is worse than labelling
it. **The causal claim rests on the interleaved phase A/B above**, which is the
controlled experiment: same binary, arms back to back, two rounds, two different
loads, and controls that do not move. The class column is the outcome, not the
evidence.

**The class still exceeds the harness's 180 s cap**, and this page does not
claim otherwise. What has changed is that "why" is no longer an open question.

### The sibling class

`io.netty.handler.codec.http2.DataCompressionHttp2Test`, whose two
`encodingTooBigMessage(padding, "snappy")` failures were re-homed here by
`http2-flowcontroller-ea-and-datacompression-snappy-20260819.md`:

| arm | result | class wall |
|---|---|---:|
| bind OFF | `found=42 ok=40 failed=2` | 33.5 s |
| bind ON | `found=42 ok=40 failed=2` | 29.2 s |
| HotSpot 25 | `found=42 ok=42 failed=0` | 2.5 s |

1.15x, and **the two tests still fail**: they carry a 5 s `serverLatch.await`
and were measured at 8 143 ms / 10 956 ms, so 1.15x does not reach it. That row
is honest about what the bind is worth: it is a real improvement and it is not
the 1.6–2.2x those two need.

### The regression gate, and a second class that flips

`-ea`, G1, 6 shards, 180 s cap, **one binary**, the 274-class
`buffer` + `codec-http` + `codec-compression` + `util` slice of the suite run
twice back to back with only the kill switch between the arms:

| | ABORTED | PASS | HANG | FAIL | NOTESTS | sum class ms |
|---|---:|---:|---:|---:|---:|---:|
| bind OFF | 19 | 218 | 18 | **4** | 15 | 2 579 474 |
| bind ON | 19 | **219** | 18 | **3** | 15 | **2 334 070** |

**Exactly one class differs across 274**, and it moves the right way:

```
io.netty.buffer.search.SearchProcessorTest  OFF  found=15 ok=14 failed=1  ms=143 469
io.netty.buffer.search.SearchProcessorTest  ON   found=15 ok=15 failed=0  ms=103 428
```

The OFF failure is `TimeoutException: testUniqueLen64Substrings … timed out
after 120 seconds` — a wall-clock budget, on a class that scans `ByteBuf`s
through the checked accessors, i.e. exactly this bind's shape. 1.39x on the
class takes it under the budget. One observation of a timeout crossing a
threshold is not a determinism claim, but it is a class that FAILS without the
bind and PASSES with it, on one binary.

Nothing else in the 274 changes status or counts, and the slice's total class
time is 1.105x.

---

## 3. What the residual actually is

`perf record -F 199` over `NettyZipBombPhases snappy 4`, bind on, G1:

| | share |
|---|---:|
| the CratonVM binary (VM runtime) | **63.5 %** |
| JIT-compiled code | 32.1 % |
| libc + unknown | 4.4 % |

That split is the finding. The predecessor page's closing sentence —
"`snappy` … measures compiled-Java throughput on a byte-shuffling loop" — is
two thirds wrong: two thirds of the phase is the VM's own per-operation
machinery, not the code it compiled.

Flat self-time, same profile, grouped:

| share | cluster | owner |
|---:|---|---|
| **13.4 %** | `G1Collector::is_addr_in_live_region` (8.70) + `is_object_address` (4.73) | heap address validation, ~198 M calls per 4 MiB (`CRATONVM_DBG=g1-live-memo`); the memo hits 99.9 %, so this is volume, not misses |
| 8.0 % | `try_jit_site_cached_native_dispatch`, `safe_native_call_impl`, `decode_dispatch_values_into`, `forward_jit_reference_args`, `jit_invoke_virtual_mic`, `coerce_native_return` | the residual native funnel — 7.47 M calls, of which `DirectByteBuffer.get(I)B` is 4.26 M and interpreted `VarHandle.get` 2.16 M |
| 7.2 % | `get_field_as`, `get_field_raw`, `coerce_field_value_for_slot`, `load_and_forward` | field-access machinery |
| ~5 % | `create_java_string` + `HashMap<String, ObjectRef>::get` + `__memcmp_evex_movbe` | **compiled `ldc "literal"`** — `compiled-ldc-re-derived-its-constant-every-execution-FIXED-20260820.md`. NOTE (2026-08-20): this cluster is real and the row it produced was NOT marginal. Replacing the pool lookup with the recorded resolution left `ldc "literal"` at 16.6 ns, unchanged — ~16.5 ns is the helper call itself, not either lookup. The `ldc <Class>` half of that page IS 4.05x. |
| 5.2 % | `varhandle_instance_field_read_bits` + `jit_varhandle_read_direct` | this page's own fix, at 58 ns/read |

### Why no combination of these closes the class

HotSpot runs `snappy 4` in ~300 ms; CratonVM takes 8 300 ms. Zeroing **every**
VM-runtime item above — all the validation, the whole residual funnel, the
field machinery, the `ldc` cluster and this page's own helper — leaves the
32 % that is compiled code, i.e. ~2 700 ms against HotSpot's 300. The class
would go from 539 s to roughly 200 s and still be over the cap.

So the remaining gap is not one defect and not one page's. It is the VM's
per-operation floor on a workload that performs ~6 native-shaped operations per
output byte, against a JIT that inlines none of them where HotSpot inlines all
of them. That is the per-call-floor and intrinsics story, and it is owned by the
pages listed under "Related" — not by a `codec-http` test class.

### The rung that is left, priced

`DirectByteBuffer.get(I)B` is now the top census entry (4.26 M per 4 MiB, ~6 %
of the phase). A thin bind for it is the same shape as this page's, with one
extra requirement: the call site's declared class is `java/nio/ByteBuffer`, not
`DirectByteBuffer`, so the helper must verify the receiver's class at run time
the way `jit_hashmap_get_direct` does. Priced at ~4 % of the phase, it was not
taken here because it does not change the class's status and the page is being
retired on the arithmetic above, not on exhaustion.

---

## 4. What was ruled out, with the measurement that ruled it out

* **The native-shadow caller seal.** 199 methods are sealed out of the JIT for
  calling a native-shadowed method. Interleaved, two rounds, same binary:
  seal on 8 174 / 8 305 ms, `CRATONVM_JIT_NATIVE_SHADOW_CALLER_SEAL=0`
  8 323 / 8 199 ms, `…INTERFACE_BLIND=0` 8 442 / 8 428 ms. Lifting the seal
  compiles **two** more methods (104 → 107 tracked) and moves nothing. The
  sealed population is cold.
* **The optimizing (IR) tier's virtual calls.** The bind cannot reach the IR
  door, so the obvious theory for the 2.16 M `VarHandle.get` calls still on the
  funnel was "they are in IR-compiled methods".
  `CRATONVM_JIT_IR_CALL_VIRTUAL=0` — which sends those methods to the
  single-pass backend where the bind does apply — leaves the funnel count at
  2 161 768 (from 2 161 936) and the wall clock unchanged. Those calls are
  **interpreted**, not IR-compiled.
* **Methods stuck in the interpreter.** `hot_but_stuck_in_interpreter=0`,
  `osr_refused_entry=0`, `c2=90` of 104 tracked methods, `deopts=0`,
  `c2_bailouts=0`. The hot code compiles.
* **The conservative JIT root scan.** `CRATONVM_DBG=jit-scan-prof` reports
  `scans=0 band_words=0` on this workload under G1. The 198 M address
  validations are not it.
* **`DirectByteBuffer` bailing to the JDK body.** `CRATONVM_DBG=dbb-elem`:
  `get(int) served=3 820 988 bailed=0`, `put(int,byte) served=179 012 bailed=0`.
  Every element access is served natively; none falls back.
* **The collector.** Unchanged from the predecessor page — G1 and ZGC are
  within the run-to-run spread on this phase.
* **A pre-existing null-coordinate divergence, which is NOT this bind's.**
  `probes/VarHandleReadOracle` prints one line that differs from HotSpot:
  `vh.get((Holder) null)` answers `0` on CratonVM where HotSpot raises
  `NullPointerException` (`varhandle_get`'s `VH_KIND_INSTANCE` arm returns
  `Ok(Some(Value::Object(None)))` for a null receiver, which unboxes to zero).
  The bind-ON and bind-OFF arms are byte-for-byte identical on that line, which
  is what says it predates this work: the helper hands a null coordinate
  straight to the generic dispatcher. Left as found, and recorded here because
  the oracle prints it.

---

## 5. Carried forward from the predecessor page

Two closed items the predecessor established, kept so retiring it does not lose
them:

* **The `ByteBuffer` wide absolute accessors are served natively** (2.2–2.7x on
  the accessor rung), and **that fix does not move this class** — the census
  said so first: this path uses the BYTE accessor, 4.26 M times, and the wide
  ones barely at all. That prediction held.
* **The IR stack-argument blocker is CLOSED.** `emit_direct_cross_call`
  marshals arguments past the register file onto the stack, pinned by
  `a_direct_call_past_the_register_file_marshals_its_tail_on_the_stack`.

And two the predecessor retired, which stay retired: `writeZero` (and the
1780x built on it) was not the cost, and netty refusing `sun.misc.Unsafe` is
what HotSpot does too.

---

## Repro

```bash
cd apps/netty-suite-runner
printf 'io.netty.handler.codec.http.HttpContentDecompressorTest\n' > /tmp/one.txt
./run-netty-suite.sh --list /tmp/one.txt --gc g1 --shards 1 --timeout 900 --out runs/repro
```

Per-test decomposition — the only form that says WHICH test the budget went to:

```bash
cratonvm --java-home <jdk> -cp <suite-cp> \
  -Djunit.jupiter.execution.timeout.mode=disabled \
  PerTestProgressRunner io.netty.handler.codec.http.HttpContentDecompressorTest
```

The A/B, on ONE binary:

```bash
CRATONVM_JIT_VARHANDLE_READ_DIRECT_HELPERS=0 cratonvm … NettyZipBombPhases snappy 8
CRATONVM_JIT_VARHANDLE_READ_DIRECT_HELPERS=1 cratonvm … NettyZipBombPhases snappy 8
```

The probes that carry the numbers above:

```bash
cratonvm --java-home <jdk> -cp <suite-cp> VarHandleReadRate 4000000 40
cratonvm --java-home <jdk> -cp <suite-cp> VarHandleReadOracle
cratonvm --java-home <jdk> -cp <suite-cp> --dump-native-registry=/tmp/reg.json NettyZipBombPhases snappy 4
CRATONVM_DBG=intrinsic-stats     cratonvm … NettyZipBombPhases snappy 2
CRATONVM_DBG=jit-method-stats    cratonvm … NettyZipBombPhases snappy 2
CRATONVM_DBG=g1-live-memo        cratonvm … NettyZipBombPhases snappy 4
CRATONVM_DBG=dbb-elem            cratonvm … NettyZipBombPhases snappy 4
```

`VarHandleReadOracle` must print HotSpot's output exactly, modulo the one
null-coordinate line in §4; it is the correctness pin for every read this bind
serves.

## Related

* `compiled-ldc-re-derived-its-constant-every-execution-FIXED-20260820.md` —
  the ~5 % this page's profile found and could not attribute to anything
  already known. Since FIXED, with one of its two halves measuring flat: the
  string rung this profile pointed at did not move, because the pool lookup and
  the recorded lookup cost the same and both are smaller than the call that
  reaches them.
* `httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md`,
  `httpresponsestatustest-exhaustive-loop-timeout-20260816.md` — the other two
  `codec-http` walls, the same mechanism.
* `every-jit-getfield-takes-the-helper-because-the-guarded-inline-check-always-fails-20260817.md`
  — owns the `getfield` half of the field-access machinery above.
* `adaptive-bytebuf-allocator-throughput-20260812.md` — the same per-entry
  transfer machinery from a different netty class, and the page that already
  measured the native-shadow seal as not a throughput lever.
* `http2-flowcontroller-ea-and-datacompression-snappy-20260819.md` — the
  sibling class re-homed here, and now measured under the bind.
