# aarch64 backend: nothing is refused any more, and no loop it compiles is ever entered

Status: OPEN, and the blocker has MOVED (h23 / h23b / h23c, 2026-09-22).
**Every opcode this backend can be asked to lower, it lowers.** What is left
is one door. Successor to
`docs/internal/retired/aarch64-backend-cannot-resolve-a-call-target-20260921-RETIRED-20260922.md`,
whose one named gap — call-target resolution — is closed, along with
`checkcast`/`instanceof` and a frame-base bug that made an allocating method's
oop maps unreadable.
Area: `jit/src/aarch64_backend.rs`
Severity: no capability gap left; an unreached compiled body, and an
unverified claim. Default builds are unaffected: the backend is opt-in
(`CRATONVM_JIT=arm64`, formerly `CRATONVM_JIT_ARM64`) and never runs on
x86-64.

**Second and third h23 passes, 2026-09-22 (lanes h23b, h23c). §2, §6 and
§3's first row are CLOSED, and the page has a new headline.** Monitors are
lowered (§2), both of §6's reference gaps are closed, and `multianewarray`
lowers at any arity (§3) -- so on a whole-VM aarch64 run of a
`synchronized` + `try`/`catch` + reference-field workload **not one method is
refused by this backend any more**.

`multianewarray` was also the last opcode `opcode_touches_shared_memory` named
without an ordered lowering, so the `compile_pass` shared-memory gate is now
INERT. The test written to announce that day fired; see §3 for why the gate
was kept rather than deleted.

What that uncovered is worth more than what it closed. With the refusals gone,
the next thing between "compiles" and "runs" turned out not to be a lowering
at all:

> **The OSR door is x86-64 only.** `compile_osr_artifact`
> (`vm/src/runtime/interpreter/jit_bridge.rs:924`) opens with
> `if cfg!(not(target_arch = "x86_64")) { return None; }`, so on aarch64 every
> OSR request fails at `stage=entry` and is retried forever. Any method whose
> hot path is a LOOP therefore never runs compiled on this backend, whatever
> the backend can lower — and the retries make `CRATONVM_JIT=arm64` measurably
> SLOWER than no JIT at all on a loop-shaped workload. See §8, which is now
> the page's real next step.

Method-ENTRY compiles are fine: they publish real aarch64 code, and h23c
established by reading `try_call_compiled_entry` that the code IS entered --
there is no architecture gate on the call path. §8 carries the full door
inventory.

What is left of this page. **NOT STARTED unless it says otherwise** -- the
ordering below is a recommendation about what to do first, not a claim that
anything in it is handled:

1. **§8, the OSR door. NOT STARTED; DESIGNED.** §8 carries the metadata,
   trampoline and door changes in enough detail to implement from, and says
   why it is the one item here that should not be written without the qemu
   suite at minimum. The only outright hole, and everything else is behind it
   -- a body reached only through a door that is shut cannot be measured.
2. **`JitLocalHandlers` (§1's residual). NOT STARTED, not designed.** The
   per-site stub, the operand-stack reconstruction at the handler's entry
   (JVMS §2.10: exactly one reference, the throwable) and label-based branch
   chain are all unwritten. Unlike §8 it needs no externally-published entry
   -- the handler is a label in the SAME body -- so more of it is structurally
   testable; but it changes exception ROUTING, which is correctness-critical,
   and its benefit today is zero because §8 keeps the bodies cold. Second for
   that reason, not because it is nearly done.
3. **The performance items in §4. NOT STARTED.** Inline caches, direct calls,
   the inline TLAB bump, inline field and typecheck fast paths, the monitors'
   uncontended CAS. Throughput work, same coldness argument.
4. **`invokedynamic` in §3. NOT STARTED, and not an aarch64 problem.** x64's
   own tier cannot keep an indy-bearing method compiled either; it has its own
   page. Listed here only because it is the last lowering this backend lacks
   that x64 has.
5. **§5's hardware ask. DEFERRED by the owner** (no Azure CLI access was
   available to provision it). Last on purpose: see §8.

**VERIFIED AGAINST A BINARY 2026-09-22**, as far as a binary can carry it, and
the gap between the two is what §5 is about. Two real AArch64 binaries were
built and run under qemu-user binfmt on the Azure host:

* the jit crate's test binary — **2823 tests, 0 failed**, including the eleven
  new ones that compile a body, publish the artifact and CALL it;
* `cratonvm` itself (`cargo build -p cratonvm-cli --release` for
  `aarch64-unknown-linux-gnu`, 6m25s), which ran a Java program to the right
  answer with `CRATONVM_JIT_ARM64=1`, reached this backend through the tier-up
  path (`bg-compile A64OopProbe.work(I)I tier=C1`) and refused the method — the
  refusal §6 is about.

What that does NOT discharge is the claim the page is named for: qemu is not a
machine, and every memory-ordering statement this backend makes is still
reasoned rather than observed. A binary ran; no AArch64 hardware did.

## The finding

After round 9 waves 10–23 this backend compiles 193 of the 202 opcode values.
It reads and writes statics, instance fields and array elements of every type
including references; it allocates objects and one-dimensional arrays; it
throws, casts, tests types, and **calls** — `invokevirtual`, `invokespecial`,
`invokestatic` and `invokeinterface`, through `jit_invoke_dispatch`.

For the first time since it was written there is no capability it lacks, and
since h23b that includes monitors and reference fields: on a real
`synchronized` + `try`/`catch` + reference-field workload it refuses nothing at
all.

The four pages before this one each named a blocker and each was retired by
building it. This one was written to name something that cannot be retired by
writing code here — no AArch64 machine has run any of it (§5). Removing the
refusals turned up something that CAN be, and that has to come first: the OSR
door is x86-64 only, so no loop this backend compiles is ever entered (§8).

## 1. The empty exception table was the population limiter — CLOSED (h23, 2026-09-22)

Every lowering here that could trap required `exception_table_empty`. The
reason was real: a trapping helper returns the `i64::MIN` deopt sentinel with
a pending exception stashed, the compiled body leaves through its epilogue,
and the interpreter's JIT-return drain needs to know exactly which bci
trapped to search THIS method's own exception table — without it, an unknown
throw pc could route into the wrong handler, or skip a `finally`.

x64 does not have this problem, and the reason turned out to be smaller than
it looked: `jit_set_throw_bci` (`vm/src/jit/helpers.rs`). Every x64 trap stub
stamps this method's own throw-site bci into a thread-local cell
(`JitSignals::athrow_bci`) before returning the sentinel, and the drain's
EXISTING bci-driven exception-table search — the same one the interpreter
itself uses — then routes correctly whether or not the table is empty.
`JitLocalHandlers` (`jit_local_handler_lookup`, entering the caught handler
WITHOUT leaving compiled code at all) is a *further*, purely performance,
optimization layered on top of that stamp — not what makes x64 correct.
aarch64 had neither piece; this closes the first one, which is the one that
decided whether a real Java program had any compilable methods in it at all:

> A method with any `try`/`catch`/`finally`, or any `synchronized` block, now
> compiles on this backend (provided every opcode it uses has an aarch64
> lowering at all — see §3 for the ones that still don't).

What landed: `Arm64Backend::emit_stamp_throw_bci`, the aarch64 twin of the x64
stamp, called on every trap edge that returns the sentinel — `putstatic`,
`getfield`, `putfield`, `aaload`/`aastore` (via the shared
`emit_bail_on_sentinel`/`emit_bail_if_refused`), all three allocations,
`invoke*`, `checkcast`/`instanceof`, and the `idiv`/`irem`/`ldiv`/`lrem` throw
path — plus `Arm64Backend::can_route_exception`, which replaced the flat
`exception_table_empty` refusal everywhere above. `athrow` and the
`ArrayIndexOutOfBoundsException` throw path needed no change at all, once
looked at closely: `jit_throw_exception` and `jit_throw_aioobe` already take
the throwing bci as a direct helper argument and stamp the same cell
themselves, so their `exception_table_empty` requirement was already stricter
than it needed to be.

The stub-sharing that `arith_throw_label`/`npe_throw_label` used (one stub per
zero-divisor guard, one per JEP-358 action, shared across every site in the
method) had to grow a bci key too: once two different bcis can each stamp a
different throw site, a stub shared across them would stamp whichever bci last
used it, which is exactly the wrong-handler bug this whole change closes. With
an empty table every bci still collapses onto one shared stub, byte-identical
to before this landed.

**Verified**, not just compiled: the full qemu jit-crate suite (2837/2837,
including the pre-existing tests updated for the now-correct non-empty-table
case) and a real try/catch loop —
`regression-suite/probes/A64ExcTableProbe.java`, which actually throws and is
caught on roughly 2/7 of its iterations, not a dead branch — run under the
built aarch64 `cratonvm` binary and compared against real HotSpot 21 on the
same Azure host. Identical output (`342685800`) on both, for every `n`/`iters`
combination tried.

**What this does NOT close**: `JitLocalHandlers` itself. This backend still
leaves compiled code and re-enters through the interpreter on every caught
exception — correct, but at the interpreted round-trip's cost (the x64 page
this optimization closed on measured ~2 900 ns per catch against HotSpot's
6.7-16 ns). Giving this backend the STAY-IN-COMPILED-CODE path is still real,
separable work: the handler table already reaches this far (`BackendRequest`
carries it architecture-independently; wiring it into `Arm64Backend` the way
`set_exception_table_empty` is wired would not be hard), but the per-site
stub, the operand-stack reconstruction at the handler's entry (JVMS §2.10:
exactly one reference, the throwable) and label-based branch-chain emission
are unwritten. **This still wants its own page when someone starts it.**

## 2. Monitors — CLOSED (h23b, 2026-09-22)

`monitorenter`/`monitorexit` now lower, through `jit_monitor_enter` /
`jit_monitor_exit`, in `Arm64Backend::emit_monitor_op`.

The shape is the one `ir_lower`'s `Op::MonitorEnter`/`Op::MonitorExit` arm
uses on x64: publish the oop map, call the helper, test the `i64::MIN`
sentinel, stamp the throw-site bci and leave through the epilogue if it fired.
**The lock-word CAS is inside the helper**, which is why
`ARM64_LOWERS_EXCLUSIVE_ACCESS` stays `false` — this backend still emits no
`LDAXR`/`STLXR` of its own. An inline uncontended thin-lock fast path with the
helper as its fallback is a real optimisation and is listed with the other
performance items in §4; it is not what "monitors are unwritten" meant.

Three things had to move with it, and each is the kind of thing that would
otherwise have shown up as a mystery refusal:

* `opcode_has_ordered_lowering` gained `0xc2`/`0xc3` — without it the
  shared-memory gate refuses them before any arm is reached;
* `needs_context` gained them, because both helpers take the VM pointer;
* **`self.calls` gained them**, which is the one that is not obvious.
  `wants_safepoint_frame` is `safepoints_enabled || allocates || calls`, and
  without a monitor term a body whose only helper call is the lock reserves no
  safepoint-id word — so `emit_helper_call_at_safepoint` refuses the method.
  A contended `monitorenter` PARKS the thread, so it is the most
  safepoint-capable call on this backend, not the least.

The receiver's possibly-remapped address needs no store-back here, unlike
`ir_lower`'s arm: `monitorenter` consumes its operand and this arm has already
popped it, and every other live reference to the same object is either an
operand (spilled and named in the map) or a reference local (homed by
`home_reference_locals_for_call`, reloaded by `reload_after_safepoint`) — both
from slots a moving collector rewrites. The `synchronized` block's own
`astore`d copy, which its generated handler unlocks through, is one of those
locals.

Pinned by `h23_monitors_lower_through_the_helpers`, which checks that a
`synchronized` body compiles with a NON-EMPTY exception table, that both
helpers are CALLED rather than merely materialized, that each sentinel edge
stamps its own bci (pc 1 and pc 3, separately), that the frame carries the
context word, and that an unwired helper still refuses by this arm with a
named reason.

## 3. Lowerings that are merely unwritten

| opcode | what it needs |
| --- | --- |
| `0xc5` `multianewarray` | **CLOSED (h23c, 2026-09-22).** Lowers at ANY arity through `multianewarray_n`, the helper another lane landed the same day. The dimension buffer is the OPERAND AREA itself -- the helper wants `dims_ptr[0..ndims]` outermost-first from ascending addresses, this frame's operand words ascend in address with depth, and JVMS pushes the dimensions outermost-first, so the deepest of the `ndims` entries already IS `dims_ptr[0]` and the spill the call performs anyway writes the buffer. The whole lowering is the one `SUB` that turns a frame offset into an address, which is what `emit_invoke` already does for `jit_invoke_dispatch`. x64 needs a copy loop into scratch words for the same call, because its operand stack is register-cached and it has four argument registers rather than eight |
| `0xba` `invokedynamic` | `JitInvokeInfo::invoke_kind` encodes virtual/special/interface/static and a call site is none of them. x64's own tier cannot keep an indy-bearing method compiled either (`a-method-containing-an-invokedynamic-cannot-stay-compiled`), so this is not an aarch64 gap so much as a VM-wide one |
| `0xc4` `wide`, `0xc8` `goto_w`, `0xa8`/`0xa9`/`0xc9` `jsr`/`ret`/`jsr_w` | x64 lacks these too; `jsr`/`ret` are dead in class files ≥ 50.0 |

**With `multianewarray` lowered, EVERY opcode `opcode_touches_shared_memory`
names now has an ordered lowering on this backend**, which is the day
`the_shared_memory_gate_still_has_something_to_refuse` was written to announce.
Its note said to delete the gate on that day; it was kept instead, and the test
inverted to pin the inert state -- the reasoning is on
`the_shared_memory_gate_is_inert_and_every_hazard_opcode_is_lowered`, and the
next person is invited to overrule it. What is left in this section is
`invokedynamic` (a VM-wide gap) and the four opcodes x64 lacks too.

## 4. Performance left on the table, deliberately

Named because each was a decision, not an omission:

* **No inline caches.** Every `invoke*` pays a full `jit_invoke_dispatch`,
  including the receiver-class resolution an MIC would have cached. This is the
  largest single cost the wave-22 design accepted, and the one to measure
  first if this backend is ever benchmarked.
* **No direct call.** A callee with a known compiled entry is still dispatched.
  The self-recursion case is the cheapest place to start.
* **The inline TLAB bump.** `new` always calls `jit_new_object`. x64 bumps the
  cursor inline and calls `tlab_post_init` only to finish the header. The
  aarch64 version needs an answer to a question x64's arrangement *assumes*:
  whether `tlab_post_init` may move the object it is handed, which arrives in
  an ARGUMENT register that no oop map covers.
* **No inline field fast paths.** Every field access is a helper call. x64 has
  inline compact/legacy arms with the helper as the fallback — possible here
  now that the fallback exists.
* **No inline `checkcast`/`instanceof` fast paths.** x64 screens arrays with a
  `KIND_TAGS == 0` compare and answers an exact-class hit from the header's
  class id. Both need this backend to read a class id out of an object header,
  which it has no other reason to do yet.
* **No inline uncontended monitor CAS** (h23b). `monitorenter`/`monitorexit`
  always call `jit_monitor_enter`/`jit_monitor_exit`, which is what
  `ir_lower`'s arm does on x64 too, so the thin-lock compare-and-swap happens
  inside the helper. An inline fast path with the helper as its fallback is
  the change that would finally flip `ARM64_LOWERS_EXCLUSIVE_ACCESS` to `true`
  — the encoders (`LDAXR`/`STLXR`, `CASAL`) have existed unused since wave 9.

None of these is worth measuring until §8 is fixed: a body that is never
entered has no cost to reduce.

## 5. The thing this page is named for

**No AArch64 hardware has run any of this.** Not one instruction this backend
emits has executed on an AArch64 machine, at any point in its existence.

That sentence has appeared on four successive pages and has never been the
headline, because there was always a capability gap in front of it. There is
not any more — but it is still not the headline, because §8 found something
between the two: no loop this backend compiles is ever ENTERED, on any host.
Hardware would measure the interpreter until that is fixed.

What exists instead:

* **qemu-user, for the jit crate.** 2831 tests, of which the round-9 waves
  added over a hundred that EXECUTE emitted code — they have caught an opcode
  mix-up, a register-budget bug and a stale-register bug that structural
  assertions passed over. Cheap: `CARGO_BUILD_TARGET=aarch64-unknown-linux-gnu
  cargo test -p cratonvm-jit --lib` builds and runs in seconds. See
  `docs/jit/aarch64-running-the-tests.md`.
* **qemu-user, for the whole VM.** `cratonvm` cross-builds for
  `aarch64-unknown-linux-gnu` in about six and a half minutes and runs a Java
  program correctly under binfmt with `CRATONVM_JIT=arm64`. As of h23b the
  tier-up path reaches this backend and is refused by nothing: a
  `synchronized` + `try`/`catch` + reference-field workload gives HotSpot's
  exact answer, and a method-entry compile publishes real aarch64 code
  (`full-compile A64CallHot.step(I)I entry=0x… len=80`). What it does NOT do
  is enter a compiled loop, because the OSR door is x86-64 only. **That is
  where the next person starts** — see §8.

What qemu is explicitly NOT: a timing oracle, a **memory-ordering** oracle, or
an errata oracle. Everything this backend does about the JMM — `LDAR`/`STLR`
for volatiles, `DMB ISH` on both sides of a volatile store through a helper,
and the argument that array accesses owe nothing because ARMv8 orders on
address dependency (ARM ARM §B2.3.2) — is reasoned, not observed. qemu-user
runs a strong-ordering model on an x86 host; it cannot fail any of it.

**The concrete ask is a machine**: an AArch64 CI runner, or one box, running
the jit suite and one Java workload with `CRATONVM_JIT=arm64`. Everything
else on this page is code someone can write. This is not. Raised with the
owner on 2026-09-22 and explicitly deferred; no Azure CLI access was available
to provision it from either h23 pass. It should be taken up AFTER §8, for the
reason given there.

## 6. The whole-VM aarch64 run — no refusals left (h23b, 2026-09-22)

Round 9 wave 23 ran the first whole-VM aarch64 program that reached this
backend. `work` — a loop that allocates a `Box`, calls its constructor, casts
it, `instanceof`s it and calls `get()` — was REFUSED, and the trace could not
say why: the aarch64 branch of `try_compile_inner` returns `None` before any
pipeline stage stamps its name, so `CRATONVM_DBG_JITC` reported only the
fallback `no-site-after-entry`, meaning "somewhere in the backend".

Every arm that declines already emits an `Arm64Instruction::Comment` naming
what it wanted. They had no reader; they have one now — a refusal prints
`[cratonvm-jitc] arm64-refused <method> — <the arm's own comment>` under
`CRATONVM_DBG_JITC`:

```
JAVA_HOME=<aarch64 jdk> CRATONVM_JIT=arm64 CRATONVM_JIT_THRESHOLD=20 \
  CRATONVM_DBG=jitc CRATONVM_DBG_JITC=1 \
  ./cratonvm --Xmx 256m --cp <dir> <Main> 2>&1 | grep arm64-refused
```

The first h23 pass ran that against `A64ExcTableProbe.work(int)` and got the
exception-table refusal §1 predicted, plus two unrelated, pre-existing gaps:

```
[cratonvm-jitc] arm64-refused java/util/concurrent/ConcurrentHashMap.tabAt(...) — getstatic at pc=0: type 'L' has no aarch64 lowering — bailing to interpreter
[cratonvm-jitc] arm64-refused java/lang/Throwable.fillInStackTrace()... — getfield at pc=1: no exact NullPointerException path, an unwired helper, or a reference field — bailing to interpreter
```

**Both are closed (h23b).** They were one gap wearing two coats: the HELPER
side had supported reference fields all along (`jit_getfield` takes
`GETFIELD_EXPECT_REFERENCE` in its third argument and returns the oop) and the
operand model has had `OperandKind::Ref` since wave 10. What was missing was
only this backend passing the flag and marking the result.

* **`getstatic` of `L`/`[`** reads the same 64-bit payload word `J`/`D` read
  and pushes it as a `Ref`, which is exactly what x64's
  `try_emit_inline_getstatic` does (`b'J' | b'D' | b'L' | b'['` share one arm
  there, followed by `mark_top_as_oop`). No load barrier is owed: every
  collector in this VM lets compiled code read a static's payload word raw. A
  `volatile` reference static gets `LDAR`, which is stronger than the plain
  `MOV` x64 emits under TSO and correct for the same reason.
* **`getfield` of `L`/`[`** goes through the helper, with the flag built by
  the ONE canonical encoder (`getfield_index_arg`) rather than the
  `field_index as i64` this site used to write by hand — which is precisely
  the rot that encoder's own doc comment warns about. No `dispatch_threw`
  probe is owed: only `J`/`D` can legitimately BE `i64::MIN`.

**A bug was found doing it.** `emit_getfield`'s HELPER-sentinel edge never
stamped its throw-site bci. It was missed when the first h23 pass widened
`can_throw_npe` from `exception_table_empty` to `can_route_exception`: the
inline null check leaves through the bci-keyed NPE stub and was always fine,
but this edge — the helper's own `i64::MIN`, for a receiver the inline check
passed — left with whatever bci a previous stamp had written. In a method with
a `try`/`catch` that is the wrong-handler bug the stamp exists to prevent.
Fixed, and pinned by `h23_getfield_stamps_its_throw_bci_on_the_sentinel_edge`.
`emit_putfield`/`emit_putfield_reference` were checked and need no such call:
they ignore the helper's result and their only trap is that same stub.

### What the run says now

`regression-suite/probes/A64SyncProbe.java` — a hot method whose whole body is
a `synchronized` block containing a `try`/`catch` that really throws, a
reference `getstatic` and a reference `getfield` walk, i.e. one probe covering
§1, §2 and both of the gaps above — run under the built aarch64 `cratonvm`:

```
A64SyncProbe sum=355166800      # cratonvm, CRATONVM_JIT=arm64, aarch64 under qemu-user
A64SyncProbe sum=355166800      # HotSpot 21.0.12+8
```

Identical, for every `n`/`iters` pair tried. Over that whole run **no
`arm64-refused` line is printed at all**, and `ConcurrentHashMap.tabAt` and
`Throwable.fillInStackTrace` — the two methods named above — both reach the
compiler without one.

**One honesty note about that zero.** No positive control for the refusal
print could be constructed in this binary: the obvious candidates
(`multianewarray` over 3 dimensions, `invokedynamic`) are refused by `jit_scan`
BEFORE the backend is reached, so they print nothing either. The zero is
reported as measured, not leaned on; the unit tests in §2 and the matching
answer above are the load-bearing evidence.

## 7. What the oop-map oracle has and has not said

Wave 21 closed the gap the previous page ended on, and it was bigger than the
page thought. An allocating method's maps were unreadable **three times over**:
the frame BASE they are addressed from was never published
(`emit_frame_record` was gated on polls), the frame SIZE every walker bounds a
band by was never published at all (`CompiledMethod::osr_frame_size` stayed
`0`, and each walker refuses the frame outright on that), and the oracle that
would have said so was armed on the POLLS switch rather than the backend one.
All three are fixed; a published base is now a term of `fully_oop_covered`, and
the claim is widened to an allocation-only method.

That is the lesson worth carrying: "the maps are published" was true at every
point, and was never the same statement as "the maps can be read".

The unit-level evidence is direct: an execution test stands where a collector
would stand, inside the allocation helper, and performs the runtime's own two
steps — the frame base from the helper the prologue called, then the safepoint
id at `[frame_base - sp_id_slot_off]` — and finds the bci of the `new`.

What is still missing is the same missing thing as §5: a run long enough, on a
machine real enough, for a *collector* to have walked one of these frames in
anger. Two practical notes for whoever does it, both learned the expensive way:

* `oop_map_audit::dump()` prints from `vm-cli`'s **normal shutdown**. A run
  killed by `timeout` prints nothing, however long it ran.
* `dump()` returns silently when it inspected no frames, so **silence is not a
  clean bill of health** — it is indistinguishable from "the oracle never
  engaged", which is exactly what the three bugs above guaranteed. Read
  `frames=` before `never_mapped=`.

## 8. The compiled body is never ENTERED for a loop-shaped method (h23b)

This is the page's real next step, and it is not a hardware question.

Measured while A/B-ing §6's probe against itself
(`CRATONVM_JIT_THRESHOLD=20` versus `100000000`, i.e. JIT on versus
effectively off), on four workloads:

| workload | JIT on | JIT off |
|---|---|---|
| `A64SyncProbe 2000 200` | 8.75 / 8.86 s | 8.72 / 8.53 s |
| `A64SyncHot` (hot loop in a `synchronized` block) | 56.5 / 52.7 s | 52.1 / 56.0 s |
| `A64PlainHot` (hot loop, no monitor) | 54.5 s | 52.9 s |
| `A64CallHot` (80 M calls of a tiny method) | **267.7 s** | **191.9 s** |

No speedup anywhere, and the call-shaped one is 40 % SLOWER with the JIT on.
The trace says why:

```
[cratonvm-jitc] bg-compile A64PlainHot.work(I)I tier=C2 optimized=true osr_bci=4
[cratonvm-jitc] OSR-compile FAILED A64PlainHot.work(I)I osr_bci=4 stage=entry — transient: counted as a failed compile and retried
```

`compile_osr_artifact` (`vm/src/runtime/interpreter/jit_bridge.rs:924`) opens
with

```rust
// x86-64 ONLY: this door calls `x64::compile_with_param_slots` directly,
// and no other backend publishes OSR entry points. On any other target it
// would publish x86-64 bytes. `cfg!` keeps the body type-checked there.
if cfg!(not(target_arch = "x86_64")) {
    return None;
}
```

So on aarch64 **every** OSR request fails immediately, at `stage=entry`, is
classified `transient`, and is retried — forever. That is both why no
loop-shaped method ever runs compiled and why the JIT-on arm is slower: the
retries are pure cost.

Method-ENTRY compiles go through a different door and get as far as
PUBLISHING -- which is as much as was established, and is deliberately not the
claim that such a body is then entered. The same run shows a real aarch64 body
published:

```
[cratonvm-jitc] full-compile A64CallHot.step(I)I entry=0x40005ec8e000 len=80
[cratonvm-jitc] c2-supersede published A64CallHot.step(I)I (epoch=6) c1=80 c2=80 outcome=unchanged epoch_bumped=true
```

`A64CallHot` is the workload where that body would have paid off -- 80 million
calls of a three-operation method -- and it is the row that got SLOWER.

**That is NOT because entry bodies go uncalled** (h23c, settled by reading
rather than by running, since the host is down). `try_call_compiled_entry`
(`vm/src/jit/helpers.rs`) transmutes the entry and calls it with no
architecture gate anywhere on the path, so a published aarch64 body is
entered like any other. The slowdown has two other causes, both already on
this page:

* the OSR hole above -- `A64CallHot`'s hot loop is in `main`, which is
  OSR-only, so it stays interpreted AND pays a failed-compile retry every trip;
* §4's dispatch costs -- every call into `step` goes out through the Rust
  dispatch (guards, the rbp/identity mirrors, root pushes) and back, which is
  far more than a three-operation method's body. That is what "no inline
  caches" and "no direct call" cost, and it is why §4 says none of those items
  is worth measuring until a compiled body is reached often enough for the
  measurement to mean anything.

### A third x86-64-only door

Found while settling the above. `vm/src/runtime/interpreter.rs:2468`, the
EAGER FIRST-CALL door, opens with the same `cfg!(not(target_arch =
"x86_64")) { return None; }` as the OSR door, and for the same stated reason
("this door reaches `x64::compile_with_param_slots` directly"). Unlike the OSR
door this one is not a hole -- its own comment says other targets "compile
through `jit::try_compile`, which selects their backend", and the tiered
manager does reach the method eventually -- so the effect is a later first
compile, not a permanent one. Worth knowing before anyone reads a cold-start
number off this backend.

So the door inventory for aarch64 is: **entry compiles work and are entered**;
**the eager first-call door is skipped** (later, not never); **the OSR door is
closed outright**, and that last one is what §8 is about.

### What a fix needs, concretely

Written out here rather than left as "port OSR", because the next lane should
implement it, not re-derive it. Read `jit/src/osr_entry.rs` (2 246 lines, all
`#[cfg(target_arch = "x86_64")]`) and `CompiledMethod::osr_enter` alongside
this.

The runtime side is already architecture-neutral and needs NO change:
`osr_enter` looks up `osr_pc_to_native[entry_pc]`, refuses a negative offset,
checks `osr_dead_mask`, and hands everything to `osr_trampoline`. Only the
trampoline and the metadata behind it are x64.

**1. `Arm64Backend` must publish `osr_pc_to_native`.** A per-bci table, `-1`
for "not an entry". The entry rule is x64's and should not be re-litigated:
a bci qualifies only at a loop header whose ABSTRACT EXPRESSION STACK IS EMPTY
(`osr_empty_stack_entry_enabled`, and JVMS makes the same demand of HotSpot) —
entering part-way through an expression would let the prologue materialise the
pending operands once for the entering iteration, which the loop can never
recompute because the pushes live above the back-edge target. This backend
already records a bci→pseudo-op position mapping for its oop maps, so the
table is a second read of machinery that exists.

**2. It must publish where each local lives.** x64 ships
`osr_local_assignments` / `osr_xmm_assignments`; the aarch64 equivalents come
straight off `local_regs` and `float_local_regs`, whose pools are
`ARM64_LOCAL_GPRS` (X19-X28) and `ARM64_LOCAL_FPS` (D8-D15) — both
callee-saved under AAPCS64, which is what makes seeding them from outside the
frame legal at all. A local with `None` is frame-homed and its slot comes from
`spill_word_offset`. `osr_frame_size` is ALREADY published
(`frame.frame_size - 16`), so that field needs nothing.

**3. An aarch64 `osr_trampoline`.** The x64 one generates a code stub at run
time (cached; see `osr_trampoline_cache`) that builds the frame, seeds the
locals, then jumps to `target_addr`. The aarch64 twin is the same shape and
nothing more exotic:

```text
    STP   FP, LR, [SP, #-16]!      ; the prologue this body expects
    MOV   FP, SP
    SUB   SP, SP, #osr_frame_size
    <for each local i not in dead_mask:>
      LDR   Xt, [Xlocals, #8*i]    ; jit_locals[i]
      MOV   X19..X28, Xt           ; or FMOV Dn, Xt, or STR Xt, [FP, #slot]
    MOV   X0, Xvm                  ; the context word, if this body takes one
    MOVZ/MOVK X16, #target_addr
    BR    X16                      ; NOT BLR: the body owns the return
```

The `dead_mask` skip is load-bearing and must be carried over verbatim: a
dead local can share its home register with a live one, so seeding it clobbers
the live owner.

**4. The door.** An aarch64 branch in `compile_osr_artifact`
(`jit_bridge.rs:924`), the way `try_compile_inner` already has one. Its
`direct_calls2` / retire-cell plumbing is x64-specific and should be SKIPPED
rather than ported on the first pass — those are direct-call optimisations, and
this backend dispatches every call through `jit_invoke_dispatch` anyway.

### The one thing that makes this different from the rest of the page

Every other item here fails SAFE. A lowering that gets something wrong refuses
the method and the interpreter runs it; that is why `multianewarray`, monitors
and the reference fields could be built and unit-tested without executing a
single aarch64 instruction.

**OSR entry does not fail safe.** It publishes an address the interpreter
JUMPS INTO mid-method with locals marshalled from outside, so a mistake is a
silent miscompile, not a refusal, and no structural test detects it. This
repository already has that exact failure on record — on x64, with execution
testing available the whole time: `osr_single_pc_entry_only`'s comment
documents `Arrays.sort(long[])` on >= 1000 elements throwing an
`ArrayIndexOutOfBoundsException` with a garbage index because artifacts
compiled for one pc were being entered at another
(`partitionDualPivot` compiled at 117, entered at 93).

So this work wants the qemu suite at minimum, and §5's machine ideally. It is
the one item on this page that should NOT be attempted blind.

Until then, the honest statement of this backend's status is: it compiles
essentially anything javac emits, method-entry compiles publish real code and
that code IS entered, and **no loop it compiles is ever entered**. Whoever picks up §5's hardware ask
should do this first — measuring a backend whose bodies are never entered
would measure the interpreter.
