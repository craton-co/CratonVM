# `cratonvm.JitCompileDecision`

The JIT's per-method admission verdict — which door asked, which backend
produced the body, and why — as a JFR event an operator can capture from a
production run.

**Why this exists.** On 2026-09-01 an audit measured `String.charAt` in a
counted loop at a flat **186-196 ns/char**, while a byte-identical body in the
same binary, on the same class file, reached **~3 ns/char** — a 90x gap that
never moved from 200,000 characters to 100,000,000. Five hypotheses for what
selected the fast body were each tested and each refuted:

| hypothesis | result |
|---|---|
| OSR entry is slower than method entry | 239 vs 205 ns/char — no |
| the callee was cold when the caller compiled | 327 vs 291 — no |
| the caller's shape decides it (six caller kinds) | 312-343, all six — no |
| the first compile is final and its context decides | 278-324, all eight rows — no |
| it is a scale threshold | flat 186-196 — no |

Five refutations, no convergence. The whole investigation was blind in one
specific way: **the compiler already computes a precise, human-readable reason
for every compile decision, and that reason reached stderr, under a debug flag,
on a rebuild.** `jit/src/lib.rs`'s admission chain builds a verdict string
(`optimize=false — the C1/fast tier was requested, not C2`, `value shape not
admitted (category2=true fp=false …)`, …) and prints it as `[ir] admission …`
only when `CRATONVM_DBG_JITC` or `CRATONVM_DBG_IR_COMPILES` is set. Worse, the
chain **omitted a term the real eligibility conjunction contained** — the
String-intrinsic pin — so a method the pin declined and a method the pin never
saw printed the identical `admitted to the optimizing pipeline`. That omission
is why five measurements could each be correct and the answer still not appear.
See `docs/known-issues/perf/string-charat-loop-cost-and-the-unsteerable-intrinsic-20260901.md`.

As a JFR event the same information is something an operator captures from a
run they already have, with no rebuild and no stderr grep, and "why is this
method slow?" stops being an archaeology exercise.

Everything below is produced by `cratonvm_jfr::jit_decision` (`jfr/src/jit_decision.rs`),
registered in `jfr/src/builtin.rs` and emitted by `cratonvm-jit`
(`jit/src/lib.rs`) through a sink the VM installs at boot
(`vm/src/vm/vm_init.rs`).

---

## Contents

1. [The event](#1-the-event)
2. [Arming it](#2-arming-it)
3. [A worked example](#3-a-worked-example)
4. [Which doors actually emit, today](#4-which-doors-actually-emit-today)
5. [One event per compile, and why `outcome` is authoritative](#5-one-event-per-compile-and-why-outcome-is-authoritative)
6. [Cost when nobody is listening](#6-cost-when-nobody-is-listening)
7. [The sink, and why it uses `try_lock`](#7-the-sink-and-why-it-uses-try_lock)
8. [What this event does *not* tell you](#8-what-this-event-does-not-tell-you)
9. [The sibling stderr instruments](#9-the-sibling-stderr-instruments)

---

## 1. The event

Name `cratonvm.JitCompileDecision` — the first `cratonvm.`-namespaced event in
the built-in registry, because it has no `jdk.*` counterpart. HotSpot's
`jdk.Compilation` reports what the compiler *did*; this reports what it
*decided*, and why.

| | | |
|---|---|---|
| Category | `["Java Virtual Machine", "Compiler"]` | where `jfr summary` files it |
| Period | `EventPeriod::BeginEnd` | duration rides in `end_time`, as every other duration built-in here does |
| Threshold | `None` | deliberate: a decision taken before the compile runs has zero duration, and any threshold would silently drop exactly the refusals this event exists to report |
| `has_thread` | `true` | the compiling thread — a background worker, or the mutator that tripped the counter |
| `has_stacktrace` | `false` | — |

### Seven fields

Seven, not eight, and that is a constraint rather than a coincidence: the root
`Cargo.toml` records that the workspace-hoisted smallvec's inline-N=8 "covers
the field-count of every built-in JFR event", `EventFields` is
`SmallVec<[EventValue; EVENT_FIELD_INLINE]>` with `EVENT_FIELD_INLINE = 8`
(`jfr/src/event.rs`), and an eighth field would spill the field vector to the
heap on every emit with no diagnostic. Class, method and descriptor are joined
into one `method` string rather than being three fields, matching
`jdk.Compilation` and leaving that headroom intact.

| # | Field | Type | Contents |
|---|---|---|---|
| 1 | `method` | `string` | `class.name+descriptor` — e.g. `java/lang/String.charAt(I)C`. Internal (slash-separated) class name |
| 2 | `door` | `string` | `MethodEntry`, `EagerFirstCall` or `Osr` |
| 3 | `outcome` | `string` | `SinglePass`, `Optimizing` or `Refused` |
| 4 | `reason` | `string` | the admission verdict on a success, the bail-site name on a refusal, or `compiled` when the compile never reached the admission chain |
| 5 | `bailBci` | `int` | bytecode index the refusal is attributable to, or `-1` |
| 6 | `bailOpcode` | `int` | opcode the refusal is attributable to, or `-1` |
| 7 | `bytecodeSize` | `int` | length of the method's bytecode |

The `method` spelling is *exactly* the one the `[ir] admission` stderr line
uses (`"[ir] admission {class}.{name}{descriptor}: {verdict}"`), so a JFR dump
and a `CRATONVM_DBG_JITC` log name the same method identically and can be
joined with no translation step.

### Two sentinels worth knowing

**`-1`, not `0`, for "not one bytecode's fault".** `note_jit_bail_site` records
`(0, 0)` for the refusals that are a property of the whole method, and `0` is
both a legal bci *and* a legal opcode (`nop`). Both emit sites map that pair
onto `NO_BAIL_SITE` (`-1`) rather than passing through a coordinate a reader
would believe.

**`bytecodeSize` is `0` at the `Osr` door**, and that is a missing measurement,
not a zero-length method. The OSR emit site
(`jit::mark_jit_bail_listed_with_site`) is handed three name strings and
nothing else; the size is out of scope there and is reported as `0` rather than
fabricated. Only `MethodEntry` events carry a real size.

### Duration is zero today

Both emit sites pass `duration_ns: 0`, so `end_time == start_time` on every
event this branch produces. The field is in the struct and the event is
registered `BeginEnd` so that a call site which *does* have a measured compile
duration can start reporting one without a schema change — and `threshold:
None` exists precisely so today's zero-duration events are never filtered out.
Do not read a `duration` of 0 in a dump as "the compile was instant".

---

## 2. Arming it

**The event is off by default, and "off" means the producer gate is false — not
that events are filtered at drain time.** The producer never builds the payload
at all.

It arms only when a **running** recording names it **explicitly**:

```java
var r = new jdk.jfr.Recording();
r.enable("cratonvm.JitCompileDecision");
r.start();
```

### The asymmetry that will surprise you

**A recording with no name filter keeps every event it sees, but it does not
arm this one.** That is deliberate, and it is the opposite of how every other
built-in behaves. `FlightRecorder::any_running_recording_names_event` is
stricter than `RecordingSettings::name_filter_admits`: a `None` filter
satisfies the latter for every event, and satisfies the former for none. The
reasoning is that arming costs the producer a formatted `String` inside the
compiler, and "keep whatever arrives" is not a good enough reason to pay it.
An operator who wants this event asks for it by name.

Concretely: `new Recording()` + `start()` with no `enable(...)` call records
nothing from this event, and the dump will contain zero
`cratonvm.JitCompileDecision` rows with no error anywhere.

### Ordering — `start()` then `enable()` works

`Recording.enable(...)` changes no recording *state*, and the gate is otherwise
recomputed by `FlightRecorder::refresh_running_ids`, which runs on start and
stop. So `r.start()` followed by `r.enable(...)` would have left the producer
permanently dark while the reverse order worked — an argument-order dependence
nothing would explain.

That is closed. Every Java-side settings edit funnels through
`jfr_configure_java_recording` (`vm/src/vm/vm_exec.rs`), and that function ends
with an explicit re-arm:

```rust
// vm/src/vm/vm_exec.rs, jfr_configure_java_recording
cratonvm_jfr::jit_decision::sync_jit_decision_gate(&recorder);
```

**Verified present on this branch** (`vm/src/vm/vm_exec.rs:17694`, in
`jfr_configure_java_recording`, with the `recording` borrow scoped above it so
`recorder` is free to be re-borrowed). Both orders arm the producer.

### What does *not* arm it

Two routes an operator would reasonably reach for, and neither works today —
check this section before writing a `jcmd` line into a runbook.

* **`-XX:StartFlightRecording`.** The flag's option set is
  `filename` / `duration` / `maxage` / `maxevents` / `dumponexit`
  (`JfrStartRecordingConfig`, `vm/src/config.rs`; parsed by
  `parse_jfr_start_recording_opts` in `vm-cli/src/main.rs`). There is no
  `settings=` and no event-enable option, and the recording `vm_init.rs`
  creates for it is a plain `RecordingSettings::new("cratonvm")` with
  `enabled_event_names: None`. It therefore **cannot name an event**, and
  cannot arm this one on its own.
* **`jcmd JFR.start`.** `JFR.start` / `JFR.stop` / `JFR.dump` are registered as
  commands but never touch the flight recorder — see the audit block at their
  registration site in `vm/src/runtime/serviceability.rs`, and note 3 in that
  module's header. The jcmd surface itself has no production caller.

**The in-process `jdk.jfr.Recording` API is the only route that arms this
event today.** The chain is: `Recording.enable(String)` → the settings map read
in `native-builtins/src/jfr.rs` → `jfr_configure_java_recording`
(`vm/src/vm/vm_exec.rs`) writes `enabled_event_names` on the VM-side Java
recording → `sync_jit_decision_gate` flips the producer gate.

---

## 3. A worked example

The recipe below is derived from the code, not from a recorded run — nothing on
this branch has been built or executed. Treat the output block as the shape to
expect, not as a transcript.

### The arming program

Arming is a Java-side act, so it happens inside the workload. For a probe you
control, arm it first and dump at the end:

```java
import jdk.jfr.Recording;
import java.nio.file.Path;

public class CharAtProbe {
    public static void main(String[] args) throws Exception {
        var r = new Recording();
        r.enable("cratonvm.JitCompileDecision");   // BY NAME — nothing else arms it
        r.start();

        runTheWorkload();                          // the compiles happen here

        r.stop();
        r.dump(Path.of("charat.jfr"));
    }
}
```

```
cratonvm -cp . CharAtProbe
jfr summary charat.jfr
jfr print --events cratonvm.JitCompileDecision charat.jfr
```

`FlightRecorder::dump_recording` writes through `jfr/src/jdk_chunk.rs`, which
emits the stock JFR format (binary element-tree metadata, 68-byte header,
unsigned-LEB128 integers) and is verified against the JDK's own
`RecordingFile`, `jfr summary` and `jfr print`. The bespoke encoding in
`jfr/src/dump.rs` is *not* on this path — do not confuse its FORMAT-FIDELITY
GAP block with this one.

### Reading the output

```
$ jfr summary charat.jfr

 Event Type                          Count  Size (bytes)
=========================================================
 cratonvm.JitCompileDecision           412         38104
 ...
```

```
$ jfr print --events cratonvm.JitCompileDecision charat.jfr

cratonvm.JitCompileDecision {
  startTime = 09:41:22.118
  method = "java/lang/String.charAt(I)C"
  door = "MethodEntry"
  outcome = "SinglePass"
  reason = "pinned to the single-pass backend: it has a String access intrinsic here and the IR tier has none"
  bailBci = -1
  bailOpcode = -1
  bytecodeSize = 25
  eventThread = "main"
}

cratonvm.JitCompileDecision {
  startTime = 09:41:22.140
  method = "CharAtProbe.scanBig(Ljava/lang/String;)J"
  door = "MethodEntry"
  outcome = "Optimizing"
  reason = "admitted to the optimizing pipeline"
  bailBci = -1
  bailOpcode = -1
  bytecodeSize = 61
  eventThread = "main"
}
```

The first row is the answer the audit spent a day not getting: the method is
**compiled**, never appears in any bail list, and is on the single-pass backend
because a named rule put it there.

### Keeping the rest of the dump

`enable(name)` installs a **name filter** on that recording, and
`name_filter_admits` then drops anything not in the enabled set. A recording
that names only `cratonvm.JitCompileDecision` therefore yields a dump
containing (essentially) only that event — which is often what you want, but is
a surprise if you expected `jdk.Compilation` beside it. Either name the other
events too:

```java
r.enable("cratonvm.JitCompileDecision");
r.enable("jdk.Compilation");
r.enable("jdk.ClassLoad");
```

…or run the arming recording alongside an unfiltered one. `-XX:StartFlightRecording`'s
recording has no name filter, so it keeps every event the process emits —
including the decision events the Java-side `enable(...)` armed:

```
cratonvm -XX:StartFlightRecording=filename=full.jfr,dumponexit=true -cp . CharAtProbe
```

The Java recording arms the producer; the CLI recording collects everything and
dumps on exit. Neither can do the job alone.

---

## 4. Which doors actually emit, today

`jit/src/compile_gate.rs` exists because patching one compile door and shipping
was a recurring failure mode here: `EagerFirstCall` and `Osr` reach
`x64::compile_with_param_slots` **directly** and never pass through
`try_compile_with_invokespecial_resolver`. So "the compiler emits a decision
event" is not uniform across the three doors, and this table is the part of the
page to read before trusting a census.

| `door` value | Emits today? | Where | Coverage |
|---|---|---|---|
| `MethodEntry` | **yes** | the completion funnel in `try_compile_with_invokespecial_resolver` (`jit/src/lib.rs`) | one event per compile, all three `outcome` values |
| `Osr` | **partly** | `jit::mark_jit_bail_listed_with_site` (`jit/src/lib.rs`) | refusals only, from **one** of the OSR bail paths |
| `EagerFirstCall` | **no** | — | the variant exists and is mapped, but no call site emits |

Consequences, spelled out because a reader will otherwise assume uniformity:

* **Every non-`Refused` event in a dump is `door = "MethodEntry"`.** The other
  two doors never emit a success.
* **`door = "Osr"` always carries `outcome = "Refused"`**, `bytecodeSize = 0`,
  and a `reason` that is a bail-site name. This is the door whose refusals
  matter most — it compiles a `@Test` method's hot loop, and a method denied
  there runs its whole life interpreted with no other diagnostic — which is why
  it got an emit of its own even though a successful OSR compile does not.
* **The `Osr` coverage is partial.** Only the backend-returned-`None` path at
  `vm/src/runtime/interpreter/jit_bridge.rs` goes through
  `mark_jit_bail_listed_with_site`. The earlier OSR bails in that file —
  `compile_gate::admit` refusing, `jit_scan` refusing, the exception-table
  gates — call the plain `mark_jit_bail_listed`, which has no emit. Those
  refusals are in the `CRATONVM_DBG=jit-method-stats` table and the
  `[cratonvm-jitc] osr-DENY …` stderr lines, and **not** in a JFR dump.
* **`door = "EagerFirstCall"` will never appear.** The interpreter's eager
  first-call compile asks `compile_gate::admit` with that door and then reaches
  the backend directly; nothing on that path calls
  `record_jit_compile_decision`. An empty count for it is a gap in the
  instrument, not a fact about the workload.

The `MethodEntry` event reads its door off `admission.door()` — the compile
gate's own token, threaded through the whole function — rather than a
thread-local mirror, so it cannot drift from `compile_gate`'s three-door table.
`jfr_compile_door` is an exhaustive `match`, so a fourth door is a compile error
at that line rather than a silent `MethodEntry`.

---

## 5. One event per compile, and why `outcome` is authoritative

**`outcome` is a fact, not a prediction** — and getting that right is why there
is exactly one event per compile rather than one per admission.

The admission verdict is built in `try_compile_inner` **before** the optimizing
pipeline runs, so it is a *prediction* of which backend will produce the body.
A method the chain admitted can still fall back to single-pass inside that
pipeline. The *fact* is `CompiledMethod::used_ir_backend`, and it is known only
at the completion funnel in `try_compile_with_invokespecial_resolver`.

Emitting at both places would put two events with contradicting `outcome`
values in the dump for every admitted method, and a reader asking `jfr print`
"which backend actually compiled this?" would have to know to join them and to
prefer the second. Instead the verdict is **carried forward** to the funnel in a
thread-local (`JIT_ADMISSION_VERDICT`) and the single event the funnel emits
pairs the authoritative `outcome` with the verdict that explains it:

```
outcome = Refused      <- result was None
outcome = Optimizing   <- used_ir_backend == true
outcome = SinglePass   <- a body, but not from the IR tier
```

The admitted-then-fell-back population is exactly the `String.charAt` shape,
and it is the population that matters: compiled, fast-ish, absent from every
bail list, and silently missing every optimization the IR tier would have
applied.

**So do not expect one event per admission.** A compile that never reaches the
admission chain still emits, with `reason = "compiled"` — which is a fact about
that compile (`optimize` was not even asked), not a gap in the instrument. And
because the verdict is built in `try_compile_inner`, only `MethodEntry`
compiles ever produce a verdict at all; the OSR emit carries a bail-site name
instead.

The verdict frame is RAII (`JitDecisionFrame`), pushed at the top of the funnel
and popped on every exit, because `try_compile_with_invokespecial_resolver` has
early `return None` exits that never reach the emit — and a frame left behind
would be read by the *next* compile on that thread. It is a **stack**, not a
cell, because compiles nest: `callee_compiler` re-enters the same function on
the same thread for an inlining candidate, and with one slot the callee's
verdict would overwrite the caller's.

---

## 6. Cost when nobody is listening

`jit_decision_enabled()` is **one `Acquire` atomic load and a branch** — the
precomputed AND of "a sink is installed" and "a running recording names the
event". On a default run it is false forever and the compile path pays nothing
else.

**The predicate is public and separate from the emit call on purpose.** The
`reason` is a formatted `String`, and a gate that could only be consulted
*after* it was built would defeat itself. The natural way to call this wrongly
is to format the reason first and then discover nobody wanted it:

```rust
// RIGHT — the gate refunds the formatting
if cratonvm_jfr::jit_decision::jit_decision_enabled() {
    let verdict = /* the String */;
    cratonvm_jfr::jit_decision::record_jit_compile_decision(&decision);
}
```

`record_jit_compile_decision` re-checks the gate, so a careless caller is still
*correct* — just not free. That second check cannot un-allocate a `String` the
caller already built.

The same discipline reaches into the verdict site itself: the admission chain
in `try_compile_inner` builds its verdict under
`ir_stage_reporting() || metrics.is_enabled() || jit_decision_enabled()`. A JFR
consumer is the **third** reader of that verdict, and without that third
disjunct the verdict would never be *built* for it — every decision event would
carry the fallback `reason = "compiled"` and the event would answer nothing.

Two further no-cost properties:

* `DecisionText::Static` carries a `&'static str` straight into
  `EventValue::Str`, skipping the per-event `Arc::from`. Every
  `note_jit_bail_site` site name is a literal, so the whole refusal path
  allocates nothing for its reason.
* `JIT_ADMISSION_VERDICT` is only ever pushed while the gate is armed, so a
  default run never allocates that `Vec` at all.

`Acquire` rather than `Relaxed` matches `cratonvm_jfr::is_enabled` and for the
same reason: the arming path writes registry state before it flips the flag. On
x86-64 it is the same instruction as a relaxed load; it is correct on AArch64,
which this VM also targets.

### One caveat on the flag's scope

It is a **producer-side cost gate, not the filter**. The authority on whether
an event is *kept* is still the per-recording name filter at drain time. That
matters because the flag is process-global while a `FlightRecorder` is not: in
the `cratonvm-vm` test binary, which builds a recorder per test, the last
recorder to call `sync_jit_decision_gate` wins. A spuriously-`true` flag costs
one sink call whose event the uninterested recording drops; a spuriously-`false`
one loses diagnostic events in a concurrent test. Production has exactly one
recorder per process, where the flag is exact.

---

## 7. The sink, and why it uses `try_lock`

`cratonvm-jit` has no `FlightRecorder`. The recorder is
`SharedVm::debug.flight_recorder` in `cratonvm-vm`, and a `jit → vm` dependency
edge would cycle. So `cratonvm_jfr::jit_decision` holds a `OnceLock` sink and
the VM fills it in at boot (`vm/src/vm/vm_init.rs`) — the same installer shape
as `cratonvm_gc::install_gc_start_hook` and `install_class_info_hook`, except
that this one is a **boxed closure** rather than a bare `fn`, because it has to
*capture* the recorder handle. It captures a `Weak`, so a sink that outlives
its VM cannot keep the VM alive.

The closure takes the recorder with **`try_lock` and a bounded spin, never
`lock()`**:

```rust
const JIT_DECISION_LOCK_ATTEMPTS: u32 = 64;
for _ in 0..JIT_DECISION_LOCK_ATTEMPTS {
    if let Some(mut jfr) = vm.debug.flight_recorder.try_lock() {
        cratonvm_jfr::builtin::emit_jit_compile_decision_event(&mut jfr, decision);
        return;
    }
    std::thread::yield_now();
}
```

`flight_recorder` is a non-reentrant `parking_lot::Mutex`, and this closure runs
on whatever thread is compiling — a background compile worker, or a mutator
part-way through executing Java. Every critical section taking that lock today
is `acquire → one emit_* → drop`, with no Java re-entry and no compile inside
it, so a self-deadlock is **not reachable as the code stands** (checked across
every `flight_recorder.lock()` site in `vm/` and `vm-cli/` when this was
written). But this sink is the first thing that can call *into* the recorder
from the middle of a compile, and it turns "some future path emits a JFR event
with the recorder held and then runs Java" from a harmless mistake into a hung
VM. That is not a trade a diagnostic gets to make: **a lost decision event is a
hole in a dump; a deadlock is a hung process.**

The spin is bounded rather than a single attempt so that ordinary contention —
several compile workers each holding the lock for one formatted event — does not
silently thin the census out and make a `jfr print` undercount. A genuine
self-deadlock costs 64 yields per compile and then continues: slow and visible
rather than silent and fatal.

**Read a suspiciously low count with this in mind.** Under heavy concurrent
compilation, events *can* be dropped after 64 failed attempts, with no record
of the drop. The event is a diagnostic, not an audited census.

---

## 8. What this event does *not* tell you

* **It names the decision, not the generated code.** `outcome = "Optimizing"`
  says the IR tier produced the body. It says nothing about what that body
  looks like, which optimizations fired inside it, or whether the result is
  good.
* **It says nothing about why a *compiled* method is slow.** If the answer is
  a bad inline decision, a spill, a missing intrinsic *inside* an admitted
  body, or a deopt loop, this event will report a clean `Optimizing` and be
  correct and useless. Reach for `docs/jit/compiler-metrics.md` and
  `docs/observability/phase-accounting.md` instead.
* **`SinglePass` is not a failure.** The String-intrinsic pin deliberately
  keeps `charAt`-shaped methods on the single-pass backend, because the
  intrinsic is worth more there than the IR tier's optimizations are —
  the measurement in `string_intrinsic_pin_enabled`'s own comment is 504 ns/call
  for the C2 body with the intrinsic dropped against 135 ns/call for the C1 body
  with it emitted. The event will report `outcome = "SinglePass"` with a
  `reason` naming the pin. **That is the answer** — but only if you know to read
  it that way, rather than as the tier having failed.
* **Absence is not evidence.** Per §4, a missing method may simply have gone
  through a door that does not emit. Check the door table before concluding a
  method was never compiled.
* **Duration is always 0** today (§1). It is not a compile-time measurement.
* **Events can be dropped under lock contention** (§7).

---

## 9. The sibling stderr instruments

This event does not replace them; it makes them unnecessary for the *production*
case. When you can rebuild and re-run, these are still richer:

* **`CRATONVM_DBG_JITC=1`** (or `CRATONVM_DBG_IR_COMPILES=1`) — `ir_stage_reporting()`.
  Prints `[ir] admission <class>.<name><descriptor>: <verdict>` for every
  candidate, plus the OSR door's own `[cratonvm-jitc] osr-DENY (…)` and
  `[cratonvm-jitc] OSR-bail site=… pc=… opcode=…` lines. The `method` field of
  this event is spelled to join against these lines directly.
* **`CRATONVM_DBG=jit-method-stats`** — the end-of-run per-method table
  (`jit/src/tiered.rs`), which names every permanently bail-listed method and
  its reason. It also prints the String-intrinsic pin's engagement census:

  ```
  [cratonvm] JIT String-intrinsic pin: fired=N blind-no-layout=N blind-no-resolver=N fail-closed=N
  ```

  A **site count and an engagement count answer different questions**, and the
  value of that line is that a zero is readable. During the audit, switching the
  pin off (`CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1`) cost nothing —
  326.3/328.6 ns/char against a default of 329.5/333.7 — which is exactly what
  "it was never on" looks like from outside, and no timing can separate that
  from "it fired and did not help". `fired=0` can, in one line.

---

## See also

* `jfr/src/jit_decision.rs` — the gate, the vocabulary, and its tests
* `jfr/src/builtin.rs` — the event declaration (entry 48) and
  `emit_jit_compile_decision_event`
* `jit/src/compile_gate.rs` — the three-door table this event's `door` field
  mirrors, and why one-door patches keep failing here
* `docs/known-issues/perf/string-charat-loop-cost-and-the-unsteerable-intrinsic-20260901.md`
  — the case study this event was built for, including the five refutations and
  the A/B that has not been run
* `docs/observability/phase-accounting.md` — where a run's wall clock went, once
  you know which methods were compiled how
* `docs/jit/compiler-metrics.md` — the per-compilation breakdown, for the
  questions this event explicitly does not answer
