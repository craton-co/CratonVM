# Lock elimination and coarsening: what is proved, what is refused

Scope: the P2 item *"Lock elimination/coarsening"* of
the C2 review — wait/notify, identity exposure,
exceptions, deopt relocking, contention.

Subject: `jit/src/escape_analysis.rs`, "Phase 4". Companion to
`docs/jit/escape-analysis.md`, which covers the reachability/value/identity
machinery both transforms sit on.

---

## 0. Status in one paragraph

> **Status, verified against the source 2026-09-12.** The paragraph below the
> rule was written before monitors reached the IR tier. What is true now:
>
> * **The IR has monitor ops and the builder emits them.** `ir::Op::MonitorEnter`
>   and `MonitorExit` carry `[ctrl, mem, obj]`. `IrBuilder` emits them for
>   `monitorenter` / `monitorexit` and threads the memory token through them.
> * **They are bridged.** `escape_analysis_from_ir` maps them to
>   `escape_analysis::Op::MonitorEnter` / `MonitorExit` and re-packs the locked
>   reference from IR input 2 to EA input 0.
> * **Lock elision runs in production, per plan.** `apply_ea_to_ir_pinned`
>   (`jit/src/lib.rs`) iterates `ea_result.lock_elisions`. For each object it
>   maps every monitor through `reverse_map` and refuses the **whole object** if
>   any monitor has no IR counterpart, is named by a safepoint snapshot
>   (`ea_snapshot_names`), cannot be spliced out of the memory chain
>   (`ea_splice_feasible`), or has its value read through a non-token input.
>   That is the §6.1 shape. Production code no longer reads the flat
>   `elide_locks`. The comment just above that loop, which says `lock_elisions`
>   is empty for every IR-derived graph and the loop is a no-op, is stale.
> * **Lock coarsening is still not consumed.** `apply_lock_coarsening` is called
>   only from `escape_analysis.rs` tests.
> * **A method whose monitors were elided cannot deopt-resume precisely.** See
>   §8.
> * **Live monitors lower through the runtime helper.** `ir_lower`'s monitor arm
>   publishes the safepoint map, calls `runtime_lowering::emit_monitor_stub`
>   (targeting the `monitor_enter` / `monitor_exit` helper slots), reloads the
>   shadow stack, and checks the `i64::MIN` sentinel. `lower_inner` refuses the
>   method with "monitor helper absent" when those slots are unwired.

Two transforms are implemented and tested here: **lock elision** (delete every
monitor operation on a provably confined object) and **lock coarsening** (merge
two adjacent lock regions on a confined object by deleting the inner
`monitorexit`/`monitorenter` pair). Both fail closed on every axis the review
names. *As first written:* neither runs in production yet, for a reason that
predates this work: `escape_analysis_from_ir` in `jit/src/lib.rs` has no arm
that can produce `escape_analysis::Op::MonitorEnter`, so an IR-derived graph
contains no monitor nodes at all and both offer lists are empty. §6 is the
bridge edit that changes that, and it must not land before §6.1.

---

## 1. The four monitor operations

`Op` gained `MonitorWait`, `MonitorNotify`, `Safepoint` and `Throw`. None has a
producer; each exists so a refusal is *intentional and reportable* rather than
an accident of the `Op::Call` rule, exactly as `RefCompare`/`IdentityHash`
already do for identity.

| Variant | Why it is not `Op::Call` |
| --- | --- |
| `MonitorWait` | requires the monitor held, releases it to the recorded reentry depth, reacquires at that depth. Elision destroys all three facts. |
| `MonitorNotify` | requires the monitor held; the thread it could wake is a thread that reached the object. |
| `Safepoint` | the only way coarsening can ask "does a deopt land in this gap?". |
| `Throw` | publishes the reference out of the frame (same rule as `Return`) *and* unwinds monitors. |

`MonitorWait`/`MonitorNotify` are also identity observations
(`find_identity_observations`), so an object with either is refused scalar
replacement as well.

---

## 2. Transform (a): lock elision

### Preconditions

| # | Requirement | Where |
| --- | --- | --- |
| E1 | Every live monitor-family node in the **whole graph** is attributable | `unattributable_monitors` |
| E2 | The object is `NoEscape` (`EscapeState::is_confined`) | `find_lock_elision_plans` |
| E3 | No live `wait`/`notify`/`notifyAll` names the object | `waits_or_notifies_on` |
| E4 | The offer names **every** monitor on the object, all-or-nothing | `LockElisionPlan` |

### JMM argument

JLS 17.4.4: a *lock action* on monitor `m` synchronizes-with the *unlock action*
on `m` that immediately precedes it in the synchronization order. That edge is
only observable between **different** threads — two synchronization actions of
the same thread are already ordered by program order, over which `hb` is
transitively closed.

E2 proves no reference to the object is reachable from any other thread for the
object's whole lifetime. Therefore no other thread can ever execute a
synchronization action on `m`; every action on `m` is this thread's; and
deleting all of them deletes no `hb` edge that constrains any legal execution.
This is the JSR-133 "synchronization on a thread-local object is a no-op"
argument, and it is why the transform may also drop the implied fences.

E4 is a *lock-state* requirement, not a memory-model one: removing a strict
subset of a balanced monitor sequence leaves a `monitorexit` with no matching
`monitorenter` (`IllegalMonitorStateException`) or a monitor held past the end
of the frame.

E3 is separate again. `wait` is the one monitor operation whose *semantics*
depend on the monitor actually being held, and none of those semantics survive
— or can even be expressed — once the monitor is gone. **An object whose
monitor is waited on is never elided**, and never coarsened either.

### E1, and why it is global

`monitor_object` resolves a monitor's operand through **φ copies only**, to
either exactly one allocation (`Allocation`), or provably not-an-allocation-of-
this-graph (`Foreign` — every path supplies an `Op::Param`, and an allocation
this frame never published is unreachable from the caller), or `Unknown`.

A single `Unknown` monitor refuses **every** lock plan in the method. It locks
*something*; if that something is an object whose other monitors we did elide,
the elision leaves an unbalanced sequence, and there is no way to tell which
object it is.

`ConnectionGraph::resolve_points_to` is deliberately **not** used for this. A
singleton points-to set is not a proof of provenance — this is the same trap
`find_scalar_replacements` documents at its φ arm. A reference input with
unknown provenance (a `Param`, a `Call` result, a field `Load`) contributes no
entry to the points-to set, so `φ(alloc, param)` resolves to the singleton
`{alloc}` while genuinely carrying the parameter on one path. Attributing a
`monitorenter` on that φ to the confined allocation and eliding it would drop a
lock that, on the parameter path, guards an object other threads share.
`Op::Other` is likewise treated as *may be a reference* even though
`is_ref_producer` excludes it: that heuristic is allowed to be optimistic
because its consumers only ever add escape.

Test: `an_unattributable_monitor_operand_poisons_every_lock_plan`, which pins
that an unrelated, perfectly confined object's lock is refused too.

### Bug found and fixed here: a monitor on an unnameable operand was elided

The previous `find_lock_elisions` read

```rust
let pts = cg.resolve_points_to(obj);
let all_no_escape = if pts.is_empty() {
    cg.get_escape(obj) == EscapeState::NoEscape
} else { … };
```

`ConnectionGraph::get_escape` reports an *unseen* node as `NoEscape`. An operand
with an empty points-to set and no escape entry — an `Op::Other` node, which is
what the bridge maps most unmodelled ops to — therefore read back as `NoEscape`
and **its monitor was offered for elision**. This is the same shape as the
"writes to an unnameable holder did not escape" defect recorded in
`docs/jit/escape-analysis.md` §2, in the lock consumer rather than the store
rule. It is latent only because the bridge emits no monitor nodes. Now an
operand that does not resolve is a refusal, not a `NoEscape`.

### Reentrancy

`lock_regions` pairs enters with exits by nesting, not by order, and reports
each region's reentry depth. `enter o; enter o; exit o; exit o` is two regions:
the outer `e1..x2` at depth 0 and the inner `e2..x1` at depth 1. Elision offers
all four monitors as one plan, so the depth is 0 before and 0 after.

`apply_lock_elision` also accepts a *partial* request when every region is
wholly in or wholly out of it — removing the inner pair of a reentrant nest is
legal and useful. It refuses `{e2, x2}`, which a naive depth count calls
balanced: those two are not a pair, and removing them would silently *narrow*
the outer region rather than eliminate a level of reentry. Test:
`partial_elision_is_accepted_only_when_whole_regions_go`.

---

## 3. Transform (b): lock coarsening

```text
  monitorenter o;  A  monitorexit o;   B   monitorenter o;  C  monitorexit o
⇒ monitorenter o;  A                   B                    C  monitorexit o
```

Exactly two nodes are deleted: the inner `monitorexit` and the inner
`monitorenter`.

### Preconditions

| # | Requirement |
| --- | --- |
| C1 | E1, E2 and E3 — attributable, confined, no `wait`/`notify` |
| C2 | `program_order_proves_dominance(graph)` — otherwise "adjacent" and "between" are not answerable in a graph with no CFG |
| C3 | The object's monitors pair up, and both regions are **outermost** (`depth == 0`) |
| C4 | Every node strictly between `first.exit` and `second.enter` is on the gap allowlist |

### JMM argument

Coarsening only ever **adds** synchronization: the gap `B` acquires an
enclosing lock region it did not have. Adding a lock region adds `hb` edges, and
adding `hb` edges can only *remove* legal executions — every execution of the
coarsened program is an execution of the original. So the memory model itself
introduces no new observable behaviour. (This is the "roach motel" direction:
moving code *into* a critical section is legal; moving it *out* is not, and this
transform never does.)

The two things that adding synchronization *can* break are not memory-model
facts, and each is closed by a precondition:

* **Liveness.** Extending a critical section can starve or deadlock a thread
  that wanted the monitor during the gap. C1/E2 make that impossible: no other
  thread can reach the object, so no thread can ever block on `m`. This is where
  the review's *contention* axis is discharged — we do not model contention, we
  prove there is none.
* **Observing the unlocked state.** The JVM offers exactly three ways to see
  that `m` is free: `wait`/`notify` on `m` (E3), `Thread.holdsLock(m)` (an
  `Op::Call`, not on the C4 allowlist), and another lock/unlock of `m` (a monitor
  op, not on the allowlist).

The removed `monitorexit; monitorenter` pair is itself a release/acquire pair on
`m`, and pairs with nothing by the elision argument in §2.

This is deliberately stronger than C2-the-compiler, which coarsens escaping
locks too. We fail closed to confined objects.

### The gap allowlist (C4)

`gap_node_refusal` is an **allowlist**, and it is an exhaustive `match`: adding
an `Op` variant without an arm is a compile error, not a silent admission.

| Admitted | Why |
| --- | --- |
| `Dead` | removed by an earlier pass; runs nothing |
| `Start`, `Const`, `Param`, `Add`, `Sub`, `Mul` | pure, total, no safepoint, no throw |
| single-input `Phi` | a degenerate copy |
| `Load`/`Store` whose holder is a **confined `Op::New` of this graph** | non-null by construction (cannot NPE) and unobservable by any other thread |

| Refused | Reason |
| --- | --- |
| `Safepoint` | `SafepointInGap` |
| `Call`, `New`, `NewArray`, `ArrayLength`, `Throw` | `MayThrowInGap` (all can raise; the first three are also safepoints) |
| any monitor op, `RefCompare`, `IdentityHash` | `ObservableGap` |
| `If`, `Merge`, `Return`, `Other`, multi-input `Phi`, a `Load`/`Store` on a foreign holder | `ObservableGap` |

### Exceptions

The requirement is *exact* unlock-on-throw behaviour. The proof is **by
exclusion**: C4 admits no node that can raise a throwable, so the gap cannot
unwind a monitor, so the unwind behaviour of the coarsened program is trivially
identical to the original's.

That is stronger than a `finally`-shaped argument would need, and it has to be.
`ir::IrBuilder::build` does not compile handler bodies at all (the "STUB-S8"
skip); an exception makes the compiled body return the `i64::MIN` sentinel and
the runtime re-runs or resumes the method in the interpreter. Whether that
sentinel path unwinds a monitor the *compiled* frame holds is a runtime property
this module cannot prove. Refusing to coarsen across anything that can throw
means we never have to.

Test: `a_throw_between_two_regions_refuses_the_merge_and_still_unlocks` asserts
the refusal, that the two regions survive untouched, and that the throw still
lies in the window where `o` is unlocked — which is what "still unlocks
correctly" means when the answer is a refusal.

### Deopt relocking

`deopt::MonitorInfo { object: FrameValue, lock_depth: u32 }` can describe holding
a monitor. It is still not enough to *repair* a coarsened gap, so we refuse.

If a deopt lands in the gap, the interpreter frame must be reconstructed with
the monitor set the original program held there — which does **not** include
`m`, because the original had already run `first.exit`. The coarsened compiled
frame **does** hold `m`. Recording `m` as held would not fix it either: the
interpreter resumes at a gap bci and goes on to execute the original
`second.enter`, reaching depth 2 with only one `monitorexit` left, so `m` is
never fully released. The reconstruction is *provably wrong*, not merely
unproven, so `Op::Safepoint` is refused in the gap
(`LockRefusal::SafepointInGap`).

Safepoints **inside** either region are unaffected and are explicitly allowed:
coarsening changes the held-monitor set only in the gap, so those frames
reconstruct exactly the monitor set they always did. Test:
`a_deopt_point_in_the_gap_refuses_the_merge_but_one_inside_a_region_does_not`
covers both halves.

---

## 4. The partial-application hazard, and which answer was chosen

`jit/src/escape_analysis.rs` *offers*; `apply_ea_to_ir` in `jit/src/lib.rs`
*disposes*. Its `elide_locks` loop independently skips a monitor when

* a safepoint snapshot slot names it (`ea_snapshot_names`),
* its memory-token chain cannot be spliced (`ea_splice_feasible`), or
* some non-token input still reads its value.

Each skip drops **one node** out of a balanced sequence.

**Choice for coarsening: safe under partial application — it depends on no
elision landing.** A `LockCoarseningPlan` deletes only its own two victims and
leaves a balanced structure whether or not any elision was applied. It is not
gated on an elision having landed, and it does not need to be.

`apply_lock_coarsening` re-verifies all four monitor nodes (kind and object)
before mutating, which is what makes that claim hold under a *hostile* graph
rather than merely a stale one. Node ids are stable and killing a node clears
its inputs, so "all four are still live monitors of the right kind naming the
same object" is exactly the statement that the structure the plan proved still
exists:

* an elision that already removed the pair ⇒ the plan is a no-op, not a double
  kill;
* an elision that removed the *outer* enter or exit ⇒ the plan is refused, so
  coarsening never compounds an imbalance it did not create.

Chained plans (three adjacent regions ⇒ two plans) share a region, so whichever
is applied first wins and the other is refused. Re-running the analysis on the
mutated graph offers the now-adjacent pair — the same iterate-to-fixpoint story
`find_identity_observations` documents for synchronized objects. Tests:
`chained_coarsening_plans_are_safe_in_any_subset`,
`coarsening_is_safe_under_partial_application_of_elision`.

**For elision the hazard cannot be closed from inside this module**, because the
consumer reads a flat `Vec<NodeId>`. What was done instead:

1. `EscapeAnalysisResult::lock_elisions` carries the offers **grouped per
   object**, which is the granularity at which they are correct.
2. `elide_locks` is derived from those plans rather than recomputed, so the two
   views can never disagree about which monitors are offered.
3. `apply_lock_elision` no longer kills whatever it is handed. It validates the
   request per object and leaves the graph **completely untouched** when the
   request is not balance-preserving. `apply_lock_elision_plan` applies one
   plan atomically.
4. The required consumer edit is §6.1.

Until §6.1 lands the in-module path is safe and the production path is
unreachable (no monitor nodes are bridged). Test:
`a_half_applied_elision_request_is_refused_whole`.

---

## 5. Interaction with scalar replacement

Unchanged and deliberately so: a **live** monitor is an identity observation, so
a synchronized object is not scalar-replaceable until its monitors are actually
`Op::Dead`. The supported route stays two-phase — analyze, apply, analyze again
— and coarsening slots into it as a third phase without changing the rule. Test:
`coarsening_then_elision_still_unlocks_scalar_replacement`.

`EscapeAnalysisStats` gained `lock_objects_elided`, `locks_coarsened` and
`locks_refused`, and `EscapeAnalysisResult::lock_refusals` records every refusal
with its reason. That is the same doctrine as `identity_blocked`: a fail-closed
answer should be measurable, not invisible.

---

## 6. Edits required outside `jit/src/escape_analysis.rs`

> **Status, verified 2026-09-12.**
>
> * **6.1 — landed.** `apply_ea_to_ir_pinned` iterates
>   `ea_result.lock_elisions` in exactly the shape below.
> * **6.2 — landed for monitors and `athrow`, not for `wait` / `notify`.**
>   `ir::Op::MonitorEnter` / `MonitorExit` exist and are bridged with the
>   input-2 → input-0 re-pack. `ir::Op::Throw` maps to `EaOp::Throw` (cov-07).
>   `Object.wait` / `notify` / `notifyAll` are still `EaOp::Call`: the
>   `// The monitor half of the bridge` comment block in `jit/src/lib.rs` says
>   they are deliberately not wired, and nothing produces `MonitorWait` /
>   `MonitorNotify`. That comment block's "NOT WIRED … `ir::Op` has no
>   `MonitorEnter`/`MonitorExit` variant" text is stale for the monitors
>   themselves. The `MemEffect::monitor_enter` doc quoted below no longer says
>   "No op produces this yet".
> * **6.3 — open.** No `EaOp::Safepoint` is produced, so `SafepointInGap` is
>   still unreachable from production IR.
> * **6.4 — open.** Nothing outside `escape_analysis.rs` reads
>   `lock_coarsening` or calls `apply_lock_coarsening`.

All in `jit/src/lib.rs`, which is another agent's file. Cited by symbol, not
line, because that file is being edited concurrently.

### 6.1 Apply elisions per plan, not per node (priority 1 — a correctness prerequisite)

`apply_ea_to_ir`, the `for &ea_lock in &ea_result.elide_locks` loop. Today it
evaluates `ea_snapshot_names` / `ea_splice_feasible` / `value_used` per monitor
node and pushes only the survivors onto `victims`. That is a per-node filter over
a set that is only correct as a whole.

Required shape:

```rust
for plan in &ea_result.lock_elisions {
    let mut group = Vec::with_capacity(plan.monitors.len());
    let mut ok = true;
    for &ea_lock in &plan.monitors {
        let ir_lock = match reverse_map.get(&ea_lock) { Some(&id) => id, None => { ok = false; break; } };
        if ir_graph.node_opt(ir_lock).is_none()
            || ea_snapshot_names(ir_graph, ir_lock)
            || !ea_splice_feasible(ir_graph, ir_lock)
        { ok = false; break; }
        let value_used = ir_graph.nodes.iter().any(|n| {
            n.op != ir::Op::Dead
                && n.inputs.iter().enumerate()
                    .any(|(i, &inp)| inp == ir_lock && !ea_is_memory_token_slot(n, i))
        });
        if value_used { ok = false; break; }
        group.push(ir_lock);
    }
    if ok { victims.extend(group.into_iter().map(|id| (id, EaVictimKind::Eliminated))); }
}
```

i.e. **one refusal refuses the whole object's monitors**, not just the monitor
that failed. `elide_locks` should then stop being read by `apply_ea_to_ir`
entirely; it stays on `EscapeAnalysisResult` for diagnostics and for the tests
that pin its shape.

This edit must land **before** 6.2. Bridging monitor ops while the per-node
filter is in place is what turns a latent hazard into an `IllegalMonitorStateException`.

### 6.2 Bridge the monitor ops (priority 2 — turns both transforms on)

`escape_analysis_from_ir` / `ir_op_to_ea_op`:

* `ir::Op::MonitorEnter` / `MonitorExit` ⇒ `EaOp::MonitorEnter` / `MonitorExit`,
  with the locked reference at input 0 (the EA layout contract, not the IR one:
  the IR node carries `[ctrl, mem, obj]`, so the bridge must re-pack it the same
  way it re-packs `Load`/`Store`). Getting that wrong attributes the monitor to
  the memory token.
* `Object.wait` / `wait(long)` ⇒ `EaOp::MonitorWait`, `notify` / `notifyAll` ⇒
  `EaOp::MonitorNotify`, on a **resolved** callee only. These are relaxations
  (they stop escaping the receiver via the `Op::Call` rule), so they must fire
  only on a resolved, known-intrinsic target; anything unresolved must stay
  `EaOp::Call`.
* `athrow` ⇒ `EaOp::Throw`.

Note that today `ir::Op` has no `MonitorEnter`/`MonitorExit` variant at all.
`ir::MemEffect::monitor_enter` / `monitor_exit` already exist and classify a
monitor as `MemOrder::Acquire`/`Release` with `safepoint: true`, but their own
doc says it plainly: *"No op produces this yet (`monitorenter` has no IR
lowering, so a synchronized method bails to the single-pass backend)."* That
bail — not the missing bridge arm — is the real gate on both transforms running.

Two things follow for whoever lands the lowering. First, `safepoint: true` on
both monitor ops is independent confirmation of §3's deopt argument: monitors
*are* deopt points, so a coarsened region's boundaries move deopt points, and
only the gap's interior needs the `SafepointInGap` check. Second, once
`monitorenter` lowers, real gaps will routinely contain safepoint-bearing ops,
so the allowlist's refusal rate will be high until §6.3 and a real dominator
query (§7.3) land.

### 6.3 Bridge safepoints (priority 3 — precision only)

`ir::Graph` carries safepoints in a side table (`ir_graph.safepoints`), not as
nodes, so `LockRefusal::SafepointInGap` is currently unreachable from production
IR. Coarsening does not become *unsound* without this — the gap allowlist admits
no node that can be a safepoint — but a bridged `EaOp::Safepoint` would make the
refusal explicit and would let a future relaxation of the allowlist stay safe.

### 6.4 Consume the coarsening plans (priority 3)

Nothing reads `lock_coarsening`. A consumer would map each plan's two victim
nodes through `reverse_map`, apply the same `ea_splice_feasible` /
`ea_snapshot_names` checks, and — crucially — refuse the **plan**, not the node,
on any failure. It should also run *before* the elision loop, since a coarsened
object is still a candidate for full elision on the next analysis pass.

---

## 7. To reconcile

1. ~~**Both transforms are unreachable in production.**~~ **Half lifted
   (verified 2026-09-12):** monitor nodes are built and bridged, so **elision**
   is reachable and applied per plan in production (see §0). **Coarsening** is
   still unreachable: nothing consumes `lock_coarsening` (§6.4). A benchmark
   showing "no change" from coarsening is still not evidence of "no effect".
2. **The `Unknown`-monitor guard is method-global.** One unattributable monitor
   refuses every lock plan in the method, including unrelated confined objects'.
   A per-object alias closure would be more precise; it is not obviously worth
   the complexity while monitors are rare, and the blunt version is the one that
   is easy to argue.
3. **Coarsening requires `program_order_proves_dominance`.** Every branchy
   method is therefore refused, which is most of them. The fix is the same one
   `docs/jit/escape-analysis.md` §9 names: give the EA graph real control edges
   and replace the program-order stand-in with a dominator query. Elision does
   **not** need it — removing every monitor is balance-preserving on every path —
   and the tests pin that asymmetry
   (`a_branch_refuses_coarsening_but_not_elision`).
4. **We coarsen only confined objects; C2 coarsens escaping ones too.** That is
   the single largest precision gap, and closing it means reasoning about
   contention and deadlock rather than proving they cannot occur. It should not
   be attempted before `Op::Safepoint` is bridged (§6.3), because an escaping
   object's coarsened gap almost always contains one.
5. **`apply_lock_elision` changed signature from `()` to `bool`.** It has no
   caller outside this module today. A caller that ignores the result silently
   ignores a refusal; consider `#[must_use]` once §6.1 lands and there is a real
   caller to hold to it.
6. **`stats.locks_elided` still counts monitor *nodes*, not objects**, so it is
   unchanged for existing consumers. `lock_objects_elided` is the new
   per-object counter.

---

## 8. Deopt after elision: no precise resume

Verified against the source 2026-09-12. **A method that had monitors elided
cannot deopt-resume precisely.** A deopt from its compiled body falls back to
the whole-method re-run. The reason is that no frame state this tier builds can
describe a held monitor.

### Why

Every `FrameState` `ir_lower` builds hard-codes `monitors: Vec::new()`. A precise
resume would rebuild an interpreter frame that believes it holds no lock. The
interpreter's own sink refuses a frame that holds monitors, but it cannot act on
information that was never recorded.

So `ir_lower::lower_inner` sets `CompiledMethod::can_deopt_resume = true` only
when all three hold: `sr_map` is set, some deopt point carries a
`VirtualObject`, **and no `Op::MonitorEnter` / `Op::MonitorExit` remains in the
graph**.

That last test alone was not enough. Lock elision runs **before** lowering and
turns the monitors into `Op::Dead`, so the post-elision graph showed none. A
guard deopt inside `synchronized (new Object()) { ... }` then resumed precisely
with no lock held, and the interpreter's `monitorexit` threw
`IllegalMonitorStateException`.

### The latch

`try_compile_inner` (`jit/src/lib.rs`) records `had_monitors` **before** escape
analysis runs:

```rust
let had_monitors = graph
    .nodes
    .iter()
    .any(|n| matches!(n.op, ir::Op::MonitorEnter | ir::Op::MonitorExit));
```

After `lower_inner` returns an artifact, `if had_monitors {
compiled.can_deopt_resume = false; }`. That holds whether or not elision
actually removed anything, and whatever the post-elision graph shows.

The single-pass backend has the same rule under a different name.
`x64/driver.rs` sets `can_deopt_resume = !deopt_points.is_empty() &&
!compiler.has_elided_monitor`, and likewise `can_osr_exit` with
`osr_exit_points`.

### What the VM does instead

Each deopt sink admits a precise resume on `(deopt_real_enabled() &&
compiled.can_deopt_resume)`, OR-ed with
`sink_precise_resume_allowed_for(...)` (`vm/src/runtime/interpreter/deopt_resume.rs`).
The second arm is additive, and for a monitor-bearing method it is always
false. `sink_precise_resume_allowed` requires all of:

* `cratonvm_jit::deopt_sink_resume_enabled()` (default on;
  `CRATONVM_JIT_DEOPT_SINK_RESUME=0` turns it off);
* the method is not `ACC_SYNCHRONIZED`;
* `!cratonvm_jit::bytecode_holds_monitor(code, code_len)`, a scan for any
  `monitorenter` / `monitorexit` opcode. Elision is a codegen decision, not a
  bytecode rewrite, so the opcodes are still there to see;
* a resume bci inside the method's code.

With both arms false, the sink takes the whole-method re-run. `ir_lower`'s
comment gives the argument: the re-run re-enters a re-entrant lock and stays
balanced. The method may still be compiled and may still deoptimize; it may not
resume **precisely**.
