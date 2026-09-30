# JIT round 14 wave 4, lane sync4 -- proposals

Follow-ups to the synchronized splice after wave 4 (SS-7 `holdsLock` fold, SY3-2 window elision
inside a region, SY3-5 census; see `jit-r14-syncsplice-proposals.md` and
`jit-r14-sync3-proposals.md`). Nothing here was built or measured by the lane.

Ranked by expected benefit over cost: **SS8-4, SS8-3, SS8-1, SS8-2, SS8-5** (IDs are stable; the
pages cite SS8-1 and SS8-2).

## SS8-4. Measure the synchronized-splice population with the census

**What.** Apply `r14w4-sync4-interp-census-sync-splice-line-patch-FIXED-20260929.md`, then run `SyncM`
(`s`, `ns`, `n`), `R12MonitorContended`, `R14Sync4HoldsLock` and one `--compatible` Spring census
under `CRATONVM_DBG_JITC=1`: the exit line `[cratonvm-jitc] ir synchronized splices: ...`, the
per-event `ir-sync-splice built` lines, and `ir-splice-refused ir-sync-splice-*` by rule.
**Benefit.** Says whether `SyncM.main`'s loop reaches the optimizing tier at all (if not, no
splice work moves `SyncM`, and the lever is the OSR door's tier choice), and which refusal rule
dominates real code (calls? multi-return? guarded virtuals, SS-6?) -- the ranking input for
everything below. **Cost.** Runs only. **Risk.** None. **First step.** The patch plus four runs.

## SS8-3. A splice window on `this` inside a synchronized instance method

**What.** `synchronized void putAll(..) { .. put(k, v) .. }` with `put` synchronized on the same
receiver: when the COMPILING method is an `ACC_SYNCHRONIZED` instance method, its body runs with
`this` held (the door's hold for a wrapped body, the self-locking prologue, or the interpreter
frame for an OSR body), and a window whose monitor is `Param(0)` -- with local 0 never stored,
`self_lock_bytecode_admitted`'s rule -- is a recursive level like SY3-2's. Elide it (both halves,
same pass), and fold `holdsLock(this)` there too. The builder knows `Param(0)`; it needs the
method's `is_synchronized` and "local 0 never stored" from the request. **Benefit.** The common
library shape (`Hashtable.putAll -> put`, `Vector.addAll`, `StringBuffer` chains) loses two CASes
per call. **Cost.** Small (one builder flag, one extra clause in
`elide_region_nested_sync_windows` and `try_fold_holds_lock`). **Risk.** Low-medium: the claim
"this body only ever runs with `this` held" must hold for EVERY way the artifact is entered --
check the dispatch cache and inline caches never CALL a wrapped optimizing body without the
door (they refuse `requires_wrapped_entry` today). **First step.** A census of how many windows
have `monitor == Param(0)` in a synchronized caller (the SS8-4 run, one extra row).

## Round 14 wave 5 (lane sync5): SS8-3 landed

`Graph::method_monitor_param` (set by `lib.rs` for an `ACC_SYNCHRONIZED` instance method,
switch `CRATONVM_JIT_IR_SYNC_METHOD_MONITOR_FACTS`, default ON) names the receiver `Param` by
index, so no "local 0 never stored" rule is needed for the window (node identity through the
finished graph's trivial φs); `try_fold_holds_lock` folds `holdsLock(this)` anywhere in the body
(inside loops only when local 0 is never written, `method_monitor_slot0_loops_below`), and
`elide_region_nested_sync_windows` deletes a window on it without a snapshot. The entry claim
was checked by reading: every publication stamps `requires_wrapped_entry = is_synchronized`
(`jit_bridge.rs`, pinned by `jit/src/tests.rs`), and every raw consumer filters on it. Census
rows `holdslock-folded-in-method`, `window-elided-in-method`.

## SS8-1. `SyncM.stepNested`: a nested same-receiver synchronized splice, multi-return

**What.** `synchronized int stepNested() { if (!Thread.holdsLock(this)) return 1; return step(); }`
is refused by two rules SS-7 does not touch. (a) The nested synchronized call: the resolver
answers no synchronized body at depth > 0 (`jit_bridge.rs` `resolve_inline_site_from`, the
`sync_splice` admission requires `nest_depth == 0`), and the scan admits no call but a folded
`holdsLock`. A nested synchronized site whose receiver node is the OPEN window's monitor needs no
pair at all (the SY3-2 argument: the outer window holds it), so it can be spliced as a plain
nested body inside the window -- the scan must then admit the nested body by the same trap-free
rules, recursively, and `append_ir_inline_site` must stop refusing `ir-sync-splice-nested` for
exactly that shape. (b) Two returns: the synchronized splice is single-return by admission; a
multi-return synchronized splice can put its ONE exit after `finish_multi_return_splice`'s join
(the lowerer's region check already admits joins inside a window as long as nothing leaves it).
**Benefit.** `SyncM`'s `n` mode (one call in eight), and nested synchronized helpers generally.
**Cost.** Medium (VM resolver + planner + builder). **Risk.** Medium: the nested body's receiver
must be PROVEN the window's node at build time, else the site must fall back to its call.
**First step.** (a) alone behind a switch, with a builder test whose nested receiver is `aload_0`
of the outer body.

## Round 14 wave 5 (lane sync5): SS8-1 landed (part b only)

(b) landed behind `CRATONVM_JIT_IR_SYNC_SPLICE_MULTI_RETURN` (default ON): scan, planner and
builder (exit after `finish_multi_return_splice`'s join); the resolver half is the exact patch
`r14w5-sync5-resolver-sync-splice-multi-return-patch-FIXED-20260929.md`. (a) was not built (it needs
a depth-1 resolver answer); its plan is item 1 of
`r14w5-sync5-sync-splice-resolver-residuals-FIXED-20260929.md`.

## SS8-2. The optimizing tier's self-locking body

**What.** Item 3 of `r13w4-sync2-self-locking-bodies-with-deopt-exits-20260928.md`; the exact plan
is its "Round 14 wave 4 (lane sync4)" section: the method monitor pushed on the builder's monitor
stack as an entry no bytecode pops (every frame then carries the hand-over shape the VM already
recognises), an exit before every `Return`, a release on every frame-less exit in `ir_lower`, a
closed-exits body check, and `self_locks_monitor` publication. **Benefit.** Synchronized methods
whose own body is hot (loops: `Hashtable.get`, `StringBuffer.append`, `Vector.indexOf`) get the
optimizing tier without the door. **Cost.** Large. **Risk.** Medium-high (a missed exit leaks a
monitor). **First step.** The builder half and the body check with the switch default OFF, and
the `emit_epilogue` audit list from the design.

## SS8-5. `holdsLock(C.class)` inside a `static synchronized` splice

**What.** SS-7 folds only instance windows: a static callee's `holdsLock(C.class)` is `ldc C;
invokestatic`, and the `ldc` of a class is a helper call the scan refuses. Peephole the pair in
the scan and the builder when the `ldc`'s class is the window's declaring class (the resolver
already resolves the class for the mirror row). **Benefit.** Small (assertion-style static
code). **Cost.** Small. **Risk.** Low. **First step.** Count the shape in the SS8-4 census before
building it.
