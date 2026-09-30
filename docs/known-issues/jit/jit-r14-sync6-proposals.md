# JIT round 14 wave 6, lane sync6 -- proposals

Follow-ups to SS8-1 a (a same-receiver synchronized body nested in a synchronized splice, spliced
with no pair) and S5-2 (`Thread.holdsLock` folded on the finished graph). Nothing here was built
or measured by the lane. Ranked by expected benefit over cost: **S6-1, S6-2, S6-3, S6-4, S6-5**.

## S6-1. Spliced `ldc` of a `Class` constant (subsumes SS8-5)

**What.** The resolver refuses every spliced body whose `ldc`/`ldc_w` names a `CONSTANT_Class`,
unnamed (`jit_bridge.rs` `resolve_inline_site_from`, the `ldc` row loop, `_ => return None`). The
caller's own `ldc C` already builds `Op::ConstClass { holder_class_id, cp_idx, slot_addr }` from
a row. Give `InlineSite` one field, `ldc_class_info: Vec<(callee_pc, holder_class_id, cp_idx,
mirror_slot)>`, filled for a class the callee's loader has already resolved (the same
no-loading rule the typecheck rows use), rebased by `append_ir_inline_site` into a new
`IrInlineTables` row the builder's `0x12 | 0x13` arm reads. SS8-5 then falls out: inside a
`static synchronized` splice, `ldc C; invokestatic holdsLock` with `C` the window's class is
`holdsLock(<the window's ConstClass>)`, which the fold can match by (holder, cp_idx) identity
without a special marker (`try_fold_holds_lock` today skips static windows).
**Benefit.** Every accessor that logs, compares or synchronizes on a class literal
(`LoggerFactory.getLogger(Foo.class)` wrappers, `Foo.class.isInstance`, `synchronized (Foo.class)`
helpers) becomes spliceable, not only the assertion shape. **Cost.** Medium: one field on
`InlineSite` touches the full literals listed on
`r14w5-sync5-sync-splice-resolver-residuals-FIXED-20260929.md` (round 14 wave 6 section). **Risk.**
Low-medium: a `ConstClass` is `Opaque` (it can load/initialise), so inside a synchronized window
the rule must admit it only for an already-initialised class (the mirror slot filled).
**First step.** Name the refusal (`no!("ldc-non-numeric-constant")`) and count it in a Spring
census under `CRATONVM_DBG_JITC=1`.

## Round 14 wave 7 (lane sync7): S6-1 landed

In its SS8-5 form only, default ON behind `CRATONVM_JIT_IR_SYNC_SPLICE_SELF_CLASS_HOLDSLOCK`.
The refusal is named (`ldc-non-numeric-constant`), and `InlineSite::ir_ldc_class_info:
Vec<(callee_pc, class_id)>` carries a spliced `ldc Class` row -- but the resolver fills it
only for a `static synchronized` body at depth 0 whose constant names its own (loaded) class,
and the JIT consumes it only as `holdsLock(C.class)` folded to `1` inside that body's window
(no `ConstClass` node is built from a row; any other row refuses the body). The row carries
the resolved class id rather than `(holder, cp_idx, slot)`: the fold needs identity, not a
materialisation. The broad case (a `ConstClass` node in an ordinary splice) is S7-1 of
`jit-r14-sync7-proposals.md`. See the wave-7 section of
`r14w5-sync5-sync-splice-resolver-residuals-FIXED-20260929.md`.

## S6-2. A synchronized method calling its own synchronized methods without re-locking

**What.** SS8-3 knows the compiling synchronized method holds `this`. A synchronized callee on
`this` that is too big or too trapping to splice (`Vector.addAll -> ensureCapacityHelper`,
`Hashtable.putAll -> put`, `StringBuffer.append -> ..`) is still a caller-held CALL or a wrapped
door entry that takes the (recursive) monitor again. When the caller provably holds the receiver
(`Graph::method_monitor_param`, or an open window / region on the same node), the call could
target the callee's RAW optimizing body (its `requires_wrapped_entry` body minus the wrapper).
**Benefit.** The Vector/Hashtable/StringBuffer "lock around own calls" pattern loses one CAS pair
per call. **Cost.** Medium-high. **Risk.** High: a deopt or exception inside the raw callee must
rebuild an interpreter frame that records the callee's own hold (the interpreter releases it on
return); the wrapped entry exists exactly for that. **First step.** Census of caller-held CALLs
whose receiver node is the method monitor (a counter in `enter_receiver_monitor`).

## S6-3. `holdsLock` in a region, on the finished graph

**What.** S5-2 folds only the method monitor. A `holdsLock(o)` whose call is dominated by a
`MonitorEnter(o)` and not by its matching `MonitorExit` on any path could fold too (the builder
already folds it mid-walk when the argument is not a pending loop φ). Needs the dominator tree
`ir_optimize` already computes for LICM. **Benefit.** Small (loops re-reading a local inside
`synchronized (o)`). **Cost.** Small-medium. **Risk.** Medium: the region's exit on an
exceptional edge must count. **First step.** Count builder-kept `holdsLock` calls inside regions.

## S6-4. Nested same-receiver bodies beyond one level, and through `invokespecial`

**What.** SS8-1 a admits exactly one level (`stepNested -> step`) through `invokevirtual`. A
private synchronized helper (`invokespecial`) is refused by the resolver's `special_sites` rule
before nesting is considered, and `a -> b -> c` all on `this` stops at `b`. Both are the same
proof one frame further (the innermost window frame, or a held frame whose own receiver was
proven). **Benefit.** Small today. **Cost.** Small for depth, medium for `invokespecial`.
**Risk.** Low (the window rule still judges every node). **First step.** Count
`ir-sync-splice-nested-shape` / `ir-sync-splice-body-calls` refusals whose body calls a
synchronized method on `this`.

## S6-5. Let a held nested body ask `Thread.holdsLock(this)`

**What.** Wave 6 admits a same-receiver nested synchronized body only when it calls nothing
(the resolver's `synchronized-nested-body-shape` refusal). Admitting its `holdsLock(this)` (which
the builder folds against the enclosing window: `try_fold_holds_lock` asks every open frame)
needs two more changes: the resolver refusal must let an `invokestatic` of
`java/lang/Thread.holdsLock(Ljava/lang/Object;)Z` through, and
`ir_splice_prune_refused_nested` must stop dropping the body -- it asks
`ir_splice_nested_entry_refused`, whose unbindable-call rule refuses a body whose only call is a
`holdsLock` row with no direct entry. Fix there: skip the unbindable rule for a body
`ir_sync_splice_folds_every_call` admits, as `append_ir_inline_site` does. **Benefit.** Small.
**Cost.** A few lines. **Risk.** Low (the window rule still judges the nested nodes).
**First step.** The prune fix plus a scan test with a nested body that folds its own
`holdsLock`.
