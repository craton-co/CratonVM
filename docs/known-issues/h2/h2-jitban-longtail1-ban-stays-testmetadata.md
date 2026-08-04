# `org/h2/` JIT ban (HIB-LONGTAIL.1) — the three regressions were ONE bug, and it is fixed

> **Which page is current.** This one. The companion
> `h2-jitban-longtail1-SUPERSEDED-20260728-ab-record.md` carries a LATER date and is
> the SUPERSEDED one — it is kept only for the A/B numbers it recorded. Both
> pages were once called `h2-jitban*-residuals-*`, so picking the right one by
> filename was a coin toss and picking by date got you the wrong one; that is
> why they are now named for what they conclude rather than when they were
> written.

**Status (2026-07-28, third pass):** residuals 1, 2 and 3 stay FIXED. The three
classes the 2026-07-28 re-test recorded as "genuine, reproducible JIT
regressions" — `TestPageStoreCoverage`, `TestReopen` and `TestRunscript` — all
had **one** root cause, it is general to the VM rather than anything to do with
H2, and it is fixed. Residual 4 (`nioMemLZF:` throughput) is 4.3x better and
still the one thing this page cannot close.

The previous revision split those three into "Bug 1" (a precise-deopt coverage
gap) and "Bug 2" (a distinct data-correctness defect), and deliberately left
Bug 1 to the precise-maps effort. Both readings were wrong in the same way:
neither is about H2, and the data-divergence and the `InternalError` are two
faces of the same escape.

## The bug: a compiled callee's deopt escaped its own call site

The MIC/PIC cascade the codegen emits at every compiled `invokevirtual` is, on
a hit, three instructions:

```asm
MOV  R11, [slot + 8]        ; cached_entry_ptr
CALL R11
JMP  .done
```

Nothing looks at the result. A compiled callee that traps returns the
`i64::MIN` deopt sentinel **and** leaves a reconstructed frame in the thread's
single stash slot, so the caller's generated code read that sentinel as if the
*caller* had deopted, and the callee's frame travelled up to whatever sink next
ran `take_last_deopt()`. That sink is running an unrelated method, so it

* de-speculated the wrong one — a trap in
  `RowDataType.read(ByteBuffer)SearchRow` was recorded against
  `BasicDataType.read(ByteBuffer,Object,int)void`, which leaves the method that
  actually keeps trapping compiled and makes an innocent one not-compilable;
  and
* could not resume a frame that was not its own, which is the
  `InternalError: precise deoptimization unavailable … refusing side-effecting
  replay` both `TestPageStoreCoverage` and `TestReopen` died with.

Refusing is the *correct* response for that sink — `RowDataType.read` has
already consumed bytes from the `ByteBuffer` it is decoding a row out of, so a
replay from entry would read the wrong bytes. The bug is that the sink was ever
handed a foreign frame.

`TestRunscript`'s script diff is the same escape landing somewhere that did not
refuse. That is why its stack carried no `RowDataType`/`DataType` frames and
looked like a separate defect: the *victim* is whichever method the leaked
stash reaches, and it varies.

Confirmed causally before any fix, on a single binary:
`CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY=0` — the switch that stops
`cached_entry_ptr` ever being written, and so the only thing that keeps the
inline call from happening — makes `TestReopen` pass.

### Why it appeared now

The inline direct-entry path is what the previous revision's headline turned
**on** (`direct_virtual_compiled_callee_entry_enabled`, default-OFF until
2026-07-27). Before that flip a compiled caller never reached a compiled callee
at all, so the escape could not happen — and lifting the `org/h2/` ban is
precisely what makes the *callers* compiled. The three regressions are the
interaction of those two changes, not a property of H2 code.

### The fix

Both generated direct-call sites — the monomorphic inline cache
(`jit/src/x64.rs`) and the shared hashed/vtable stub
(`jit/src/runtime_lowering.rs`, which is the one H2 actually hits, because
`MVMap` reads every row through the megamorphic `DataType.read`) — now check
the sentinel and route it to `jit_service_callee_deopt`:

```asm
CALL R11
MOV  R11, i64::MIN
CMP  RAX, R11
JNE  .after
<arg0 = vm, arg1 = info, arg2 = &args, arg3 = n>
CALL jit_service_callee_deopt
.after:
JMP  .done
```

The helper performs exactly the servicing every helper-mediated dispatch arm
already did (`try_resume_trapped_callee`, then the implicit-exception routing),
and returns the sentinel unchanged when it declines — so every path it does not
service behaves as before. Cost on the hit path is a `MOV imm64`, a `CMP` and a
not-taken branch.

**A blunter fix was tried first and rejected**, and the numbers are worth
keeping: refusing to publish an inline entry for any callee that can deopt
(`jit_entry_publishable`) is correct and two lines, but it costs
`TestFreeSpace` 156s → 241s — that class is the one the direct-entry
optimisation was landed for.

Two more defects fell out of the same investigation, both fixed here:

* **The PIC megamorphic arm of `jit_invoke_virtual_mic` returned the sentinel
  unhandled** where the monomorphic arm directly below it handled it. They now
  share one helper so they cannot drift again.
* **`try_resume_trapped_callee` demanded full descriptor equality with the call
  site**, which a covariant-return bridge can never satisfy: the site names
  `RowDataType.read(ByteBuffer)Object` and the method that traps is
  `RowDataType.read(ByteBuffer)SearchRow`. Matched on the parameter list
  instead; everything downstream is driven by the stash's own self-describing
  key.

### And the frame map itself was imprecise

Even once a frame reached a sink that could use it, `build_deopt_frame_inner`
refused it: local slot 5 recorded as `Unsupported`. `classify_local_kinds` is a
whole-method scan, and `RowDataType.read` reuses slot 5 across two disjoint live
ranges —

```
 45: istore 5      // int len       (indexes == null branch)
 98: astore 5      // int[] indexes (the other branch)
```

— so the slot is `Ambiguous` for the *method*, and unambiguously `int` at bci
50, which only the first branch can reach. Added a forward reaching-kind
dataflow (`refine_ambiguous_local_kinds`) over the same successor relation the
precise oop-mask pass uses, tracking only the slots the whole-method pass gave
up on. `Unknown` is the lattice bottom and merges away (a path that never
writes the slot leaves it undefined, and JVMS verification guarantees no load
of an undefined slot); exception-handler entries are seeded TOP; a refined
`Ref` stays `Unsupported`, because the flow-sensitive oop mask keeps sole
authority over ref-typed slots. Four unit tests in `jit/src/x64.rs`.

This is the "precise-maps/deopt coverage gap" the previous revision declined to
touch. It is real, and it is fixed — but note that it was **not sufficient on
its own**: with the map precise and the escape still open, all three classes
still failed identically.

## Diagnostics added — all permanent and env-gated

* The refusal message now names **which** of its three gates refused, the
  stashed key, the inline-caller depth and the reason. "Precise deoptimization
  unavailable" has three completely different causes needing three different
  fixes, and the message distinguished none of them.
* `CRATONVM_DBG_DEOPT` now names **the sink** that consumed each stashed frame
  and what method that sink was running. The stash is one thread-local slot
  with four consumers, and nothing recorded which one took it — that single
  line is what turned this from "a precise-maps gap" into "the frame is
  reaching the wrong consumer".
* `CRATONVM_DBG_DEOPT` also reports why `try_resume_trapped_callee` refused
  (it has eight early-returns and announced only success).
* `CRATONVM_DBG_DISPATCH_TALLY` — a per-callee histogram at the two general
  resolvers an inline-cache hit is supposed to bypass. A profile of a
  dispatch-bound workload is a flat tail of `slot_for_exact` samples that says
  nothing about *what* is being dispatched, which is the whole diagnosis; see
  residual 4 below, where this named the answer in one run.
## Where the ban stands — it STAYS, for exactly ONE class now

Full 218-class A/B, same binary, arms run **one at a time**:

| arm | PASS | FAIL | HANG | CRASH |
|---|---|---|---|---|
| ban in place | 155 | 18 | 45 | 0 |
| ban lifted | **162** | 22 | 31 | 3 |

Lifting is **+7 PASS** — the first time it has ever been net-positive on this
page (the 2026-07-27 revision measured 166 banned vs 155 lifted). Nine classes
go HANG/FAIL → PASS: `TestCompatibility`, `TestRunscript`,
`TestCacheLongKeyLIRS`, `TestNestedJoins`, `TestCache`, `TestCompress`,
`TestIntPerfectHash`, `TestKeywords`, `TestStringCache`.

Read the totals with care — the banned arm ran at load average 15–25 and the
lifted arm started at 38. That asymmetry inflates the lifted arm's HANGs, so it
makes +7 a floor, not a ceiling. Every per-class *change* was re-run in
isolation anyway, which is what the verdict rests on.

Of the nineteen per-class changes, seventeen are "fails one way in one arm and a
different way in the other" (`HANG→CRASH`, `HANG→FAIL`, `FAIL→HANG`) — both
arms failing, no information about the ban. `TestLargeBlob`'s `CRASH` is an
`OutOfMemoryError: young gen exhausted` at the runner's 1 GB heap, not a JIT
fault. `TestMvccMultiThreaded2` is the known load-flapper this page has
recorded before.

That leaves **one** real regression, and it reproduces perfectly in isolation:

| binary | ban in place | ban lifted |
|---|---|---|
| pre-fix (`origin/dev`) | PASS 3/3 | **FAIL 3/3** |
| this revision | PASS 3/3 | **FAIL 3/3** |

`org.h2.test.jdbc.TestMetaData` — and it is **pre-existing**, not introduced
here: the pre-fix binary fails it identically. Earlier revisions never isolated
it because their lifted arms had so many other failures that it never stood out.

### `TestMetaData` — a general VM bug with a 60-line witness

```
ClassCastException: class org.h2.value.ValueRow cannot be cast to
class java.lang.Comparable
  at org.h2.test.jdbc.TestMetaData.testQueryStatisticsLimit(TestMetaData.java:1401)
```

Bisected with `CRATONVM_JIT_DENY`, one step at a time:
`org/h2/command/query/` → `SelectGroups`. That class does

```java
groupByData = new TreeMap<>(session);   // SelectGroups$Grouped.reset()
```

where the session is the `Comparator<Value>` and the keys are `ValueRow`, which
is deliberately NOT `Comparable`. So the exception says exactly one thing: the
map lost its comparator and fell back to natural ordering.

It has nothing to do with H2. `probes/TreeMapCmpProbe.java` reproduces it in
pure JDK code, in seconds:

```bash
javac -d probe apps/h2database-suite-runner/probes/TreeMapCmpProbe.java
<binary> --java-home /data/data/jdk25-real -c probe TreeMapCmpProbe
# HotSpot:  PASS TreeMapCmpProbe (40000 iterations)
# CratonVM: FAIL TreeMapCmpProbe: 39498 failures
```

What is established:

* **It is JIT-only.** `--nojit` passes 40000/40000. The failures begin at
  iteration ~503, i.e. exactly when the allocating method tiers up
  (`CRATONVM_DBG_JIT_COMPILED` confirms it is the caller that compiles, not
  `TreeMap.<init>`).
* **The comparator is never stored**, rather than stored and lost:
  `new TreeMap<>(CMP).comparator()` returns `null` from compiled code.
* **The constructor is never called at all.** Instrumenting both TreeMap
  constructor natives shows 518 calls in the interpreted prologue and **zero**
  afterwards — neither the 1-arg native nor the no-arg one. `TreeMap.<init>`
  also never appears in `CRATONVM_DBG_DISPATCH_TALLY`.
* **It is specific to `TreeMap(Comparator)`.** `TreeSet(Comparator)`,
  `ArrayList(int)`, `HashMap(int)` and `StringBuilder(String)` are all clean in
  the same probe, so it is not a general "compiled invokespecial drops its
  arguments" defect.
* **It is not** escape analysis (`CRATONVM_DISABLE_SCALAR_REPLACEMENT=1`),
  inline `new`, `dup`/`dup_x1` handling, LICM, IR-call lowering, inline
  get/putfield, or either direct-entry dispatch switch — all measured, all still
  fail. Making the map escape (store it in a static) does not change anything
  either.
* The two known ctor-elision paths are correctly guarded: both
  `pending_ctor_sites` collectors require `desc == "()V"`, and
  `is_elidable_construction` already refuses native-shadowed constructors (the
  2026-07-27 HashMap fix). So it is a *third* mechanism, not yet identified.

Fixing it is its own investigation and is not attempted here. It is worth
noting what it implies, though: any JIT-compiled `new TreeMap<>(cmp)` anywhere
in the VM silently produces a natural-ordering map. H2 is simply the first
workload that keys one with a non-`Comparable` type and therefore gets an
exception instead of a wrong order.

**When it is fixed, this ban should lift.** Everything else is already in place:
+7 PASS, nine classes recovered, and no other isolated regression.
## Residual 4 — `nioMemLZF:` throughput — 4.3x better, still the open item

The previous revision root-caused this correctly: `FileNioMemData` stores pages
as `ByteBuffer.allocateDirect`, so `CompressLZF` uses its `(ByteBuffer, …)`
overloads, which move **one byte per `DirectByteBuffer` call** — ~65,000
interpreted invocations per 64 KB page. It named two candidate fixes and did
neither.

`CRATONVM_DBG_DISPATCH_TALLY` over 20 operations says which calls, which is the
part that turns a "throughput project" into an edit:

```
524152  invoke_or_native java/nio/DirectByteBuffer.session()…
518664  invoke_or_native java/nio/DirectByteBuffer.nextPutIndex()I
518663  invoke_or_native jdk/internal/misc/ScopedMemoryAccess.putByte(…)
  5488  invoke_or_native java/nio/DirectByteBuffer.nextGetIndex()I
```

The hot accessor is the **relative** `put(byte)` (`CompressLZF.expand` writes
its output through it), not the absolute `get(int)` the previous revision
named — and each call is a try/finally around five nested invocations, so the
caller pays six dispatches per byte, not one.

All four accessors — `get()`, `get(int)`, `put(byte)`, `put(int, byte)` — are
now served by natives that read `address`/`limit`/`position` off the receiver
and do one raw access (`native-io/src/direct_buffer.rs`). Every case they do
not fully model — unresolvable layout, an index outside `[0, limit)`, a
read-only receiver, an address the memory layer declines — bails to the real
bytecode via `invoke_virtual_bytecode_only`, so the JDK's exception semantics
are untouched. The relative pair commits `position` only *after* the access
succeeds, because the bytecode they bail to runs `nextGetIndex()` itself and
committing first would advance the cursor twice.

Writer-only `LzfProbe`, 100 operations, `nioMemLZF:1:`:

| | ms/op |
|---|---|
| HotSpot | 0.44 |
| CratonVM before | 390 |
| CratonVM after | **91** |

No regression on the other three prefixes (`memFS:`, `nioMemFS:`, `memLZF:`
are unchanged or slightly better). Witness:
`regression-suite/src/RDirectBufferElem.java` — 444 checks covering sign
extension, slice/duplicate offset, limit-not-capacity, the relative cursor
including "does not advance when the access throws", read-only refusal, and
bulk/element agreement. It passes on HotSpot and on **both** CratonVM arms by
design: this is a throughput substitution, so "identical to the bytecode it
replaces" is the entire contract.

**What is left.** 91 ms/op against HotSpot's 0.44 is still ~200x, and the
remaining cost is no longer a mystery either — the profile is now honest
interpreter work: ~29% inline-cache-to-native call machinery
(`pop_coerced_invoke_args_virtual` + `resolve_method_metadata` +
`safe_native_call_impl`), ~15% `ctx.get_field` (three or four per access,
each a layout lookup plus a descriptor coercion), ~10% the `Unsafe` arena's
`RwLock`+`BTreeMap` probe, ~14% the LZF bytecode itself. `TestFileSystem`'s
`testConcurrent` does 10,000 operations, so `nioMemLZF:` still cannot finish
inside the 300 s cap, and `TestFileSystem` still HANGS in **both** arms — which
is why it is not evidence for or against this ban.

Closing the rest is not more of the same shaving; ~200x is what an interpreted
call per byte costs, and the only structural answer is to stop making the call:
a JIT intrinsic that lowers `DirectByteBuffer` element access inline, which in
turn wants `ByteBuffer.allocateDirect` memory to be a real pointer rather than
a tagged arena handle. That is a bounded project and it is worth far more than
this test. Measured, for whoever picks it up: JIT-compiling the LZF loops
*without* such an intrinsic makes `nioMemLZF:` **worse** (444 ms/op with the
ban lifted, vs 91 with it in place) — the JIT-to-native call is more expensive
than the interpreter's inline-cache-to-native call, so the intrinsic is not
optional.

While fixing this: `copy_from_native_memory` / `copy_to_native_memory`
classified arena handles with `unsafe_arena_contains`, a **liveness** test, so
a tagged handle whose arena had already been freed fell through to the
raw-pointer branch and was `memcpy`'d as an OS address — a SIGSEGV on exactly
the use-after-free the single-element `Unsafe.get*(long)` natives already
refuse. Classifying by the tag bit refuses it instead, and costs one `AND`
rather than two locked map probes.

## Reproducing

### The callee-deopt escape (seconds)

```bash
cd apps/h2database-suite-runner
./run-h2-suite.sh discover          # required in a fresh worktree; meta/ is not committed
TMPDIR=/data/tmp H2_ROOT=/data/data/h2database/h2 CRATONVM_BIN=<binary> \
  CRATONVM_JIT_ALLOW_PACKAGES='org/h2/' OUTROOT=<out> \
  ./run-h2-suite.sh run --category all --only 'TestReopen' --tag repro
```

CRASH on a pre-fix binary in ~10 s; PASS after. `CRATONVM_DBG_DEOPT=1` prints
the sink that consumed each stashed frame, which is the line that identifies
this bug. `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY=0` makes a pre-fix
binary pass, which is the causal control.

### The `nioMemLZF:` gap (minutes)

```bash
H2CP=/data/data/h2database/h2/target/classes
javac -cp $H2CP -d /data/tmp/probe apps/h2database-suite-runner/probes/LzfProbe.java
TMPDIR=/data/tmp <binary> --java-home /data/data/jdk25-real \
  -Dprobe.reader=false -c $H2CP:/data/tmp/probe LzfProbe 'nioMemLZF:1:/probe' 100
```

Swap the prefix for `nioMemFS:1:`, `memLZF:1:` or `memFS:1:` for the comparison
row. `CRATONVM_DBG_DISPATCH_TALLY=1` prints the callee histogram.

### The full suite

`TMPDIR=/data/tmp` is required on the Azure host: `/` is full and the runner's
internal `mktemp` silently produces empty results otherwise. The same full root
filesystem breaks `cargo build`, so build with `TMPDIR=/data/tmp/build`.

Run comparison arms **one at a time**, and give every run its own working
directory. `TestRunscript` writes its database under `$PWD/data`, so two runs
sharing a directory corrupt each other's fixtures — this revision reproduced
that accidentally and briefly mistook it for the residual it was chasing. The
suite runner already isolates per class; hand-run repros do not.

## Related

- `vm/src/jit/helpers.rs` — `jit_service_callee_deopt`,
  `handle_compiled_callee_deopt_sentinel`, `descriptors_match_modulo_return`.
- `jit/src/x64.rs` — `emit_inline_callee_deopt_check`,
  `refine_ambiguous_local_kinds`.
- `jit/src/runtime_lowering.rs` — `emit_callee_deopt_check` (the megamorphic
  stub; the site H2 actually hits).
- `native-io/src/direct_buffer.rs` — the four `DirectByteBuffer` element
  accessors and what they deliberately refuse to model.
- `vm/src/jit/skip_list.rs` — the `HIB-LONGTAIL.1` entry.
- `docs/known-issues/h2/h2-jitban-longtail1-SUPERSEDED-20260728-ab-record.md` — the
  re-test whose three "distinct" regressions this page collapses into one.
