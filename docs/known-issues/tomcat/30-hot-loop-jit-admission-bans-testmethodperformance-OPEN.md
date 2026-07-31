# Three conservative JIT-admission bans leave `TestMethodPerformance`'s whole hot path interpreted

**Status:** 🔴 **OPEN.** Residual of
[24](../../internal/fixed-suite-bugs/tomcat/24-stringcache-oom-under-load-FIXED.md) (whose `OutOfMemoryError` is FIXED).
This is a *throughput* residual, in the family of
[31](../../internal/fixed-suite-bugs/tomcat/31-synchronized-code-never-jit-compiled-FIXED.md) (and of the retired
[04](../../internal/fixed-suite-bugs/tomcat/04-embedded-server-throughput-wall-CLOSED.md) /
[29](../../internal/fixed-suite-bugs/tomcat/29-throughput-wall-recurrence-and-unconfirmed-CLOSED.md)) — but unlike those it
is root-caused here to three **specific, named** admission gates, all of which
were added deliberately to close real silent-corruption bugs.

## Symptom

`org.apache.tomcat.util.http.TestMethodPerformance` runs 6 × 100 000 000
iterations of `mb.setBytes(...); mb.toStringType();` and then 6 × 100 000 000
of `Method.bytesToString(...)`. HotSpot finishes the class in **41.2 s**.

Measured end-to-end on the post-[24](../../internal/fixed-suite-bugs/tomcat/24-stringcache-oom-under-load-FIXED.md) binary,
from the class's own printout:

```
.MessageBytes conversion took :3820342393100ns      (CratonVM, 1st 100M loop)
MessageBytes conversion took :3092470156300ns       (CratonVM, 2nd 100M loop)
MessageBytes conversion took :6573830400ns          (HotSpot, same loop)
```

**3100-3800 s vs 6.6 s for the same 100M iterations — ~470-580x**, stable
across loops rather than a warm-up artefact. Phase 2 (`Method.bytesToString`,
no allocation) is worse still: 1793-2277 s per 100M loop against HotSpot's
0.40 s, ~4800x.

Run to completion, the class **PASSES** — in **30 149 s (8.4 hours)** against
HotSpot's 41.2 s:

```
Time: 30,149.543

OK (1 test)
```

So nothing here is a functional defect any more; it is purely a throughput
gap, and that gap is ~730x on the class as a whole. Before the bug-24 fix this
was all masked: the run died with a spurious OOM at ~150-600 s and never
reached a timeout. That same run is the end-to-end confirmation for bug 24.

## Root cause — three independent admission bans on the same hot path

The first two are visible in one `CRATONVM_DBG_JITC=1` run of the class; the
third is in the probe table below.

### 1. The driving loop is permanently OSR-denied (RBC.7 `invokedynamic` ban)

```
[cratonvm-jitc] bg-compile org/apache/tomcat/util/http/TestMethodPerformance.testGetMethodPerformance()V tier=C2 optimized=true osr_bci=15
[cratonvm-jitc] OSR-compile FAILED org/apache/tomcat/util/http/TestMethodPerformance.testGetMethodPerformance()V osr_bci=15 — method marked OSR-denied for the rest of this process
```

`compile_osr_artifact` (vm/src/runtime/interpreter.rs) refuses any method
containing `invokedynamic`:

```rust
if !scan.indy_ops.is_empty() { return None; }
```

`testGetMethodPerformance` contains two, from the
`System.out.println("MessageBytes conversion took :" + duration + "ns")`
string concatenations *after* each loop (Java 9+ lowers `+` on strings to
`invokedynamic StringConcatFactory`). The ban exists for a real reason — see
`docs/internal/jit-osr-loop-duplicate-execution-silent-corruption-FIXED.md`:
the 0xba arm lowers every indy site to an unconditional deopt trap, and for an
OSR frame that bail resumes at the *stale* pre-OSR back-edge, silently
re-executing a loop whose side effects were already committed. The RBC.7
comment even names this exact shape ("a `System.out.println("..." + n + ...)`
immediately following the loop") as the motivating case.

The cost is that the test method is a once-invoked harness method with the hot
loop inline, so OSR is the *only* way it can ever run compiled. Denied, all
600 000 000 iterations of loop control run in the interpreter.

### 2. `StringCache.toString(ByteChunk, …)` never compiles (RBC.6 handler-safety gate)

```
[rbc6-dbg] try_compile_inner: local_handler_reads_unsafe_local=true for org/apache/tomcat/util/buf/StringCache.toString(Lorg/apache/tomcat/util/buf/ByteChunk;Ljava/nio/charset/CodingErrorAction;Ljava/nio/charset/CodingErrorAction;)Ljava/lang/String;
[cratonvm-jitc] compile-bail org/apache/tomcat/util/buf/StringCache.toString(...)Ljava/lang/String; backend_attempted=false
```

`local_handler_reads_unsafe_local` (jit/src/lib.rs) conservatively refuses a
method whose exception handler reads a local it has not itself written. The
`synchronized (bcStats) { … }` block in `StringCache.toString` compiles to a
javac-generated monitor handler that does exactly that (it reloads the monitor
local to `monitorexit` it), so the method is refused.

> **This gate is analysed canonically in
> [23](23-charsetcache-pathological-slowdown.md)**,
> which reached it independently the same day via
> `CharsetCache.getCharset` and went further: RBC.6 fires on *ordinary*
> try/catch too, because only **parameter** slots count as reconstructible, and
> the `precise_exception_frames` escape hatch whitelists only
> `invokestatic`/`monitorenter`/`monitorexit` — so one `invokevirtual` inside
> the try region is enough. Read 23 before touching this gate; the two docs
> describe one bug with two victims.

This one sits in the *middle* of the per-iteration call chain — `toStringType`
→ `ByteChunk.toString` → **`StringCache.toString`** → `ByteChunk.toStringInternal`
— and both of its neighbours DO compile, so every iteration pays a
compiled→interpreted→compiled transition. Note the hot path never even enters
the synchronized block: `tomcat.util.buf.StringCache.byte.enabled` is false by
default, so `bcCache` is null and the method falls straight through to
`toStringInternal`. The ban is purely static.

The bail used to be **silent**: it reported only `backend_attempted=false`
under `CRATONVM_DBG_JITC`, which reads like a transient resolver miss. Fixed
here — the refusal now names itself on the default trace as
`resolver-bail site=rbc6-handler-reads-unsafe-local`. The fastest first check
for this whole shape is `CRATONVM_DBG_JIT_METHOD_STATS=1`, which prints
`hot_but_stuck_in_interpreter=N` with the offending method and its
`tier_fail_count`.

## Supporting measurements (isolated probes, this host, JDK 25 HotSpot control)

> **Caveat on absolute rates.** This Windows box was multitenant throughout
> (17 concurrent `cratonvm` processes from other sessions at one point), so
> treat every absolute figure below as a *lower bound* — see
> `feedback_shared_host_multitenant_confound`. The HotSpot control ran under
> the same conditions, and the structural findings above (OSR-denied,
> RBC.6-refused, constructor-refused) are compile-time facts read out of a trace, so
> neither depends on host load.

Per-operation cost in a JIT-compiled loop, nanoseconds. HotSpot's figures are
escape-analysed for the allocating rows, so treat those as a floor rather than
a like-for-like ratio; the CratonVM *column* is the interesting part.

| probe | what it does | HotSpot | CratonVM |
|---|---|---|---|
| `arith` | no call, no allocation | ~0 | 1 |
| `scall` | one `invokestatic` | ~0 | 7 |
| `vcall` | one `invokevirtual` | ~0 | 56 |
| `pcall` | one `invokespecial` (private method) | – | 60 |
| `directSet` | `putfield` on an old receiver | – | 54 |
| `allocArr` | `new int[1]` | – | 126 |
| `allocNoCtor` | `new Plain()` (empty ctor) | – | 162 |
| `allocArg` | ctor takes an arg, empty body | – | 278 |
| `allocNEsc` | allocation inside a **C1**-compiled callee | 3 | 105 |
| `allocEsc` | same allocation **inline in the C2/OSR loop** | 3 | 2331 |
| `allocBody` | `new Body()` whose ctor writes one field | – | 2338 |

Two leads fell out of this table. Both are now root-caused:

* **an allocation whose constructor has a body costs ~20× one whose
  constructor is empty (2338 vs 162 ns)** — root-caused to a THIRD admission
  ban, and the biggest of the three. See the section below; and
* ~~a C2/OSR loop that mixes an allocation with another helper-call op can
  enter an **endless OSR recompile loop**~~ — **FIXED** (`fix(jit): stop the
  OSR recompile loop on a permanently un-enterable entry pc`). The
  `allocPutOld` probe logged **200 `OSR-compile` events for 200 000
  iterations**; the new `OSR-recompile reason=` trace attributed 199 of them to
  `cached-cannot-enter-at-pc`. The back-edge path treated "published artifact
  that cannot be entered at this pc" as *still compiling* and re-requested a
  compile forever, instead of consuming the bounded per-pc rejection budget
  that already exists for it. Enterability is a pure function of the bytecode
  and the entry pc — the codegen writes `-1` for a pc strictly inside a
  LICM-hoisted loop body, whose pre-header an OSR entry would skip — so the
  retries were guaranteed to reproduce it. Now 200 -> **1** compile, with
  legitimate OSR (compile-then-reuse) unaffected. The loop still runs
  interpreted; this removed the wasted compiles, not the interpretation.

## 3. Constructors that store a field are never compiled — the biggest lever

The "ctor with a body costs 20×" lead above is not an allocation cost at all.
`classify_init_complexity` (`vm/src/jit/skip_list.rs`) marks any `<init>`
containing `putfield`, `putstatic`, `monitorenter/exit` or `invokedynamic` as
`InitComplexity::Complex`, and `should_skip_jit_with_init` then refuses it with
`SkipReason::Constructor`. A constructor that assigns a field — which is what
constructors are *for* — is therefore never compiled and, unlike the RBC.6
case, never even **enqueued**:

```
allocNEsc (static callee):  tiered-enqueue CallRate.make(I)… invoc_count=500  -> compiled
allocBody (ctor callee):    (nothing — no tiered-enqueue, no bg-compile, ever)
```

So every `new` whose constructor stores a field runs that constructor in the
interpreter, forever: `new String(…)`, `new HashMap.Node(…)`, essentially the
whole JDK. This is a VM-wide ceiling on allocation, not a Tomcat issue.

**Status: the ban is now LIFTED BY DEFAULT** (2026-07-28, explicit maintainer
decision, with a full regression run to follow). It applies to `putfield`-only
constructors; `putstatic` / `monitorenter-exit` / `invokedynamic` constructors
remain banned. **Kill switch — no rebuild needed:**

```bash
CRATONVM_JIT_PUTFIELD_INIT=0
```

restores the historical blanket ban. If a regression run turns up a
miscompile, wrong result or crash, set that and re-run *before* anything else:
it is the fastest attribution test for this change and separates it cleanly
from everything else in the same binary.

**Measured prize:**

| probe | ban on (default) | ban lifted |
|---|---|---|
| `allocBody` (ctor assigns one field) | 1846 ns | **226 ns** (8.2×) |
| `allocArg` (ctor, empty body) | 200 ns | 186 ns |
| `allocBare` | 100 ns | 103 ns |
| `allocNoCtor` | 86 ns | 106 ns |

Controls flat, so the knob does only what it claims.

**Correctness evidence so far.** `CtorCheck` (a probe that READS BACK every
field — plain stores, a superclass ctor storing before a subclass ctor, a store
whose value comes from an instance method call on the half-built object, and a
conditional store) gives byte-identical checksums across HotSpot, CratonVM
`--nojit`, ban-on and ban-lifted, at both 200k and 2M iterations. Six JIT test
binaries pass with the ban lifted (`jit_interp_differential`,
`jit_collection_ctor_identity`, `jit_local_exception_handler_tests`,
`jit_null_receiver_npe`, `jit_arity_5plus`, `jit_category2_params` — 26 tests).
A six-class Tomcat sweep produced **no attributable regression**: 3 PASS, and
all 3 non-PASS reproduce identically with the knob OFF.

**What is NOT yet established.** The ban predates the open-source import
(`a6dc911ed`) and is one of the four *structural* bans; unlike the ~46 named
correctness bans it carries no incident write-up, only the one-line "field
stores trigger the JIT's load-forwarding interaction". Nothing here proves that
rationale stale — it proves only that six Tomcat classes and 26 JIT tests do
not catch it. The full-suite regression run (Tomcat + Spring Boot + Hibernate)
**on a quiet host** is the real verdict and is still outstanding; the kill
switch above exists precisely because of that. If it comes back clean, delete
the `putfield` arm from `classify_init_complexity` outright and retire the
knob; if it does not, the failing case is the incident write-up this ban never
had — record it here.

> **Warning to anyone measuring this.** `TestDefaultServlet` has a
> **pre-existing flaky stack overflow** on `dev` under load — an unmodified
> baseline binary crashed once in four runs with
> "thread 'main-vm' has overflowed its stack" while this host was running three
> concurrent heavy jobs. It ate an entire investigation cycle here: a single
> crash-vs-pass pair was read as attribution three separate times (to the
> constructor ban, then to an OSR change, then to a debug `eprintln!`), and
> every one of those was refuted by simply repeating the baseline. **Repeat the
> control before believing any difference against this class.**

## What a fix would involve

Neither ban should simply be relaxed — each closed a confirmed
silent-corruption bug, and the corruption they prevent is invisible (wrong
results, not crashes). Plausible directions, roughly in order of
value/risk:

1. ~~**Make the OSR recompile loop stop**~~ — **done**, see the struck lead
   above. It was pure waste elimination and needed no safety property relaxed.
2. **Link `invokedynamic` in compiled code** instead of lowering it to an
   unconditional trap. That removes RBC.7's premise rather than its check.
3. **Narrow RBC.6** — but see
   [23](23-charsetcache-pathological-slowdown.md)
   first, which spells out why the obvious narrowing (widening
   `precise_exception_frame_sites_supported` to `invokevirtual`) is *unsafe*:
   several invoke lowerings in `jit/src/x64.rs` deliberately bypass
   `emit_post_invoke_exception_check`, and inlined callees never reach the
   caller's check, so widening reintroduces silent-wrong-locals. Doing it right
   means auditing every invoke lowering to publish the reason-9 frame first.
4. ~~Make the silent bails self-reporting~~ — **done**, see the trace line
   above.

## Reproduction

```bash
CP=$(cat apps/tomcat/.suite/cp.txt)
CRATONVM_DBG_JITC=1 CRATONVM_DBG_RBC6=1 <cratonvm.exe> -Xmx2g -cp "$CP" \
  org.junit.runner.JUnitCore org.apache.tomcat.util.http.TestMethodPerformance \
  2>&1 | grep -iE 'TestMethodPerformance|StringCache'
```

Both diagnostic lines appear within the first ~30 s, long before the class
would finish.

## Update 2026-07-27 — ban 2 (RBC.6) is lifted; ban 1 (RBC.7) remains

`precise_exception_frame_sites_supported` now admits `invokevirtual` /
`invokespecial` / `invokeinterface` alongside `invokestatic` and the monitor
ops, so the RBC.6 refusal of `StringCache.toString` is gone. The widening was
gated on auditing every lowering the `0xb6 | 0xb7 | 0xb9` codegen arm can
select — two of its four exits emit no call at all, the inline path is already
unreachable under `precise_exception_frames` (`inline_sites.clear()`), and the
remaining two both end in `emit_post_invoke_exception_check`. The sibling
tail-call, which tears the frame down before the callee runs and so escapes the
handler, is now suppressed inside protected ranges
(`x64::Compiler::pc_is_protected`); that was a pre-existing hole for
`invokestatic`, which this whitelist always admitted.

This addresses direction 3 of "What a fix would involve" above — though by
proving the *lowerings* safe rather than by pattern-matching the javac
`synchronized` shape, which is strictly more general.

**This does not on its own close this document.** Ban 1 (RBC.7 — a method
containing `invokedynamic` is permanently OSR-denied) is untouched, and
`testGetMethodPerformance` is a once-invoked harness method whose hot loop is
inline, so OSR remains the only way it can run compiled. Expect the
compiled→interpreted→compiled transition through `StringCache.toString` to be
gone and the per-iteration cost to drop, but the loop control itself still
interprets until RBC.7's premise is removed (direction 2: link
`invokedynamic` in compiled code instead of lowering it to a trap).

Sibling doc `23-charsetcache-pathological-slowdown.md` shares this gate and was
half-closed by the same change (its `timeFull < timeNone` assertion now passes).

## Update 2026-07-30 — ban 1's premise is removed, and a SECOND gate is found behind it

Direction 2 ("link `invokedynamic` in compiled code instead of lowering it to a
trap") is now implemented, narrowly, for `StringConcatFactory` sites. **RBC.7 no
longer refuses `testGetMethodPerformance`.** It still does not OSR — because a
second, completely independent gate refuses the same entry, and that gate was
invisible for as long as RBC.7 bailed first.

### What was built

A resolved concat call site is lowered to a direct call instead of an uncommon
trap:

* `vm/src/runtime/invokedynamic.rs` — `make_jit_string_concat_site_from_parts`
  resolves a bootstrap to a process-lived `JitStringConcatSite` (recipe +
  constants + target descriptor), or `None` for every other bootstrap kind.
  `execute_jit_string_concat_raw` decodes the JIT's raw i64 arg buffer **through
  the call site's descriptor** before any Java code runs, so a category-2 value
  is never reclassified from its bit pattern.
* `jit/src/x64.rs` — the `0xba` arm calls that bridge when a site resolved;
  every other bootstrap still falls through to the existing trap.
* `vm/src/runtime/interpreter.rs` — RBC.7's blanket
  `if !scan.indy_ops.is_empty() { return None; }` becomes a *bridged-only*
  admission, evaluated after `indy_info` resolves. A method with any unbridged
  indy is still refused, and logs
  `[cratonvm-jitc] osr-DENY (unbridged invokedynamic)`.

`has_indy_trap` is now computed from *unbridged* sites only, so a fully bridged
method no longer forces its callers onto the dispatch helper.

### The result — ban 1 is cleared, and it was not the only blocker

```
[cratonvm-jitc] bg-compile OsrConcatProbe.main([Ljava/lang/String;)V tier=C2 optimized=true osr_bci=4
[cratonvm-jitc] indy-concat bridge pc=25 args=1
[cratonvm-jitc] indy-concat bridge pc=56 args=1
[cratonvm-jitc] OSR-reject OsrConcatProbe.main([Ljava/lang/String;)V entry_pc=4 (dead_mask non-zero; memoed)
```

The bridge fires, no `osr-DENY` is logged — RBC.7 passed. The OSR body then
*compiles successfully* and is refused at the door by
`CompiledMethod::can_osr_enter`, which rejects any entry pc whose
`osr_dead_mask` is non-zero. Identical on the real shape
(`OsrMessageBytesProbe`, `entry_pc=24`, bridge at `pc=62`).

**So the doc's original root-cause list was incomplete.** Removing RBC.7 does
not make `testGetMethodPerformance` OSR; it only moves the refusal one gate
later. Anyone measuring direction 2 in isolation and seeing no speedup should
look here before concluding the bridge is broken.

### The second gate, precisely

`CRATONVM_DBG_OSR_META=1` prints the published mask:

```
[osr-meta] gpr_resident=0xb xmm_resident=0x0
           blanket_entries=[(0,a),(4,1),(a,1),(15,9),(23,1),(29,1),(34,9)]
           published_entries=[(0,8),(4,1),(a,1),(23,1),(29,1)] unblocked=2
```

At `entry_pc=4` the mask is `0x1` — local 0 (`args`). It is **dead** at the loop
head but register-resident, and it shares a home GPR with a live local
(graph-colouring coalescing reused the register once `args`'s range ended).
Entering there would load the interpreter's stale `args` over the live local's
register.

The obvious fix — *don't load dead locals at entry* — *is already implemented*
in the OSR trampoline (`jit/src/lib.rs`, the `dead_mask >> i & 1` `continue`,
added 2026-06-03 in `62e65640f`), and it is **not sufficient**. The guard that
currently short-circuits it was added **after**, on 2026-07-03 in `3415d052b`
("fix(jit): reject unsafe OSR dead-local entries"), whose own write-up says
skipping the load "still left the OSR-entered compiled frame relying on a
coalesced state transition that was not proven safe". The root problem is that
`osr_local_assignments` is a **whole-method** table: it cannot express "at this
pc this register belongs to local *j*, not local *i*". Two tests pin the
refusal (`test_can_osr_enter_rejects_dead_masked_entry`,
`test_osr_enter_rejects_dead_mask_before_trampoline`).

**Do not simply delete that guard.** Making this entry safe means giving OSR a
per-pc local→location map, not relaxing the check.

### Correctness evidence for the bridge

`ConcatBridgeProbe` drives 14 distinct `makeConcatWithConstants` shapes from a
hot loop — `int`, `long` (full 64-bit, `(i<<33)^i`), `double`, `float`, `char`,
`boolean`, `byte`, `short`, a null `String`, a null `Object`, mixed arity with
interleaved category-2 values, and the no-constant / bracketed forms — and FNV
checksums every string produced:

| | acc | sample |
|---|---|---|
| HotSpot JDK 25 | 510489415044571348 | `m=199999:1717978328665407:99999.5:s7` |
| CratonVM, JIT | 510489415044571348 | identical |
| CratonVM, `--nojit` | 510489415044571348 | identical |

19 `indy-concat bridge` lowerings were logged in the JIT run, so the bridge is
genuinely on the measured path rather than being bypassed.

`OsrConcatProbe` (the exact loop-then-`println("..."+total)` shape RBC.7 was
written to protect) returns `first=12499997500000 second=24999995000000`,
matching HotSpot — no duplicate loop execution.

Five Tomcat classes A/B against a pure-`origin/dev` binary built on the same
host: `TestMessageBytes` (8), `TestByteChunk` (8), `TestCharChunk` (3),
`TestStringCache` (1), `TestCookieParsing` (8) — all `OK`, identical both sides.

Throughput is neutral, as expected while the second gate still blocks OSR —
interleaved A/B, `OsrMessageBytesProbe` 500k, quiet host (load 0.8):

| round | base (ns) | new (ns) |
|---|---|---|
| 1 | 8 697 986 374 | 8 530 225 215 |
| 2 | 8 474 439 293 | 8 748 237 515 |
| 3 | 8 573 803 178 | 8 585 377 862 |

~0.5% apart on the means, inside the baseline's own 2.6% run-to-run spread.

### Deliberately NOT ported from the handover branch

The originating worktree (`codex/fix-tomcat-hotloop-jit-admission-20260728`,
based 187 commits behind) also carried three changes that were **dropped** here:

1. **The RBC.6 admission-gate rewrite.** It deleted the `return false` in
   `precise_exception_frame_sites_supported`, leaving an empty `if` whose
   comment still claims it checks something — i.e. the gate admits everything.
   That is a much larger safety change than this doc needs, it re-opens what
   `a523715a8` ("Fix Spring Boot residual exception-handler cluster") closed on
   2026-07-29 by re-admitting `0xb6`/`0xb9`, and it collides head-on with the
   still-unmerged `codex/fix-tomcat-charsetcache-complete-20260729-019fb049`,
   which re-widens the same line *with* a regression test. RBC.7 is a separate
   gate; none of this was required.
2. **The generic protected-range exact-resume trap** in `x64.rs`, which existed
   only to justify (1).
3. **`RETIRED_COMPILED_METHODS`** — process-lifetime retention of every
   superseded compiled body. `dev` already solves that problem properly with
   `defer_jit_owner` (drop immediately when no JIT execution is in flight,
   otherwise hold until `ACTIVE_JIT_EXECUTIONS` hits zero).

`DIRECT_CALLEE_EXCEPTION_ROUTE_TAG` (admitting exception-table callees as
tagged direct calls instead of refusing them) was also left out — it is an
independent optimisation, not part of ban 1.

### Unrelated breakage found on `dev`

`cargo test -p cratonvm-jit` **does not compile on `origin/dev`**: 11 integration
tests fail with `E0063: missing fields service_callee_deopt and set_throw_bci in
initializer of JitRuntimeHelpers`. Identical count on a pure-`origin/dev`
checkout and on this branch, so it predates this work — but it means that suite
is currently unavailable as a regression gate for anyone touching the JIT.

### Status

Ban 1 (RBC.7) — **premise removed** for string-concat sites; the ban now only
covers unbridged bootstraps. Ban 2 (RBC.6) — see doc 23, unchanged here. Ban 3
(constructor) — unchanged. **This document stays OPEN**: the 730x class-level
gap is untouched, and the next lever is no longer an admission ban at all but
the per-pc local→location map that `can_osr_enter` needs.

## Adopted 2026-07-31 — two residuals from the retired tomcat/32

[32](../../internal/fixed-suite-bugs/tomcat/32-doc04-residual-perf-assertions-CLOSED.md)
closed; two of its items are this document's family and move here with their
numbers. **Both come with a correction to this document's own framing**: in
neither case do the hot methods fail to compile. They compile, and the compiled
output is ~100x off HotSpot. "Hot methods never compile" is the right story for
`TestMethodPerformance`'s OSR-denied driving loop; it is the wrong story for
these two, and reading them through it sends the work at admission gates that
are not the problem.

### 30.A — `juli.TestOneLineFormatterPerformance.testDateFormat` (was 32.4)

Asserts `DateFormatCache` beats `String.format`, 10^6 iterations each. The test
feeds `System.nanoTime()` to a formatter cached on `time / 1000`, so it misses
essentially every call and the miss path — a bare `SimpleDateFormat.format` —
is what is measured. End to end 2026-07-31, loaded host:

```
StringFormatImpl        4 730 855 700 ns
DateFormatCacheImpl   606 794 187 200 ns      -- 128x short
```

`CRATONVM_DBG_JITC` shows both hot methods compiling —
`SimpleDateFormat.format` at `len=1708`, `subFormat` at `len=64359` — so this
is codegen quality, not admission:

| operation | HotSpot | CratonVM |
|---|---|---|
| `SimpleDateFormat.format` (same Date) | 2.3–2.5 µs | 250–300 µs |
| `DateFormatSymbols.getInstance(Locale.US)` | 1.1–1.2 µs | 50–62 µs |
| `new DateFormatSymbols(Locale.US)` | 0.5–1.1 µs | 28–34 µs |

Closing it needs `SimpleDateFormat.format` at ≲ 4.7 µs, which is where
`String.format` lands **because on CratonVM that side is a Rust intrinsic**. The
assertion therefore reduces to "compiled Java must match a Rust intrinsic", and
**optimising `String.format` makes this test harder to pass** — worth knowing
before anyone treats the fast side as an improvement target.

Separately actionable but *not* a lever for this test:
`java/text/DateFormatSymbols.getProviderInstance` fails codegen
(`compile-bail … backend_attempted=true`, `tier_fail_count=3`), which
`CRATONVM_DBG_JIT_METHOD_STATS` classifies as "not policy — these are bugs". It
is reached ~2x per `String.format` call, i.e. it is on the fast side.

Repro: `cratonvm.exe -Xmx2g -cp <probes-out> DateFmtProbe` and
`… DateSymbolsProbe 1000`.

### 30.B — `TestAsyncMessagesPerformance`'s SEQ2 residual (was 32.3)

32.3's binding SEQ1 assertion was a real defect and is **fixed** (`9f7095ed9`,
the bulk `ByteBuffer` natives copying one byte per accessor call): SEQ0 4→1 and
SEQ1 500→86–143 across interleaved reps. What remains is SEQ2 — the gap between
the 16 KiB message and the 4 KiB message, tolerance 100, actual 495–500 — and
it is here because it is measured to be general Java throughput.

`CRATONVM_DBG_AIO_INLINE` (`9bef50216`) splits the ~1.3 ms gap, n=1000
not-ready reads:

```
wait buckets: <1ms=493  1-10ms=6  >10ms=501  (sub-10ms mean=444us)
queue_mean=35us   deliver_mean=46us
```

* **81 µs** is our AIO plumbing (35 queue + 46 deliver) — about 6 %,
* **444 µs** the worker genuinely blocked waiting for the peer,
* **~775 µs** client-side Java between the two `onMessage` callbacks.

Thread wake-up is ruled out too: a Semaphore round-trip is 20.5 µs against
HotSpot's 10.5 µs and `park`/`unpark` is *faster* than HotSpot at 8.1 vs 10.3 µs
(`probes/ParkPingPongProbe`). The test runs the embedded server and the client
in one process, so the 444 µs peer turnaround is also our VM executing Tomcat's
send path. **No further AIO or buffer work will close SEQ2.**
