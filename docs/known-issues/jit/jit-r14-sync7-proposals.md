# JIT round 14 wave 7, lane sync7 -- proposals

Follow-ups to SS8-5 (`holdsLock(C.class)` in a `static synchronized` splice, through S6-1's
rows) and RV6-1 (method-monitor facts for `static synchronized` methods). Nothing here was
built or measured by the lane. Ranked: **S7-1, S7-2, S7-3, S7-4**.

## S7-1. A general spliced `ldc Class` (the rest of S6-1)

**What.** Wave 7 fills `InlineSite::ir_ldc_class_info` only for a `static synchronized` body's
own class, and the JIT never builds a `ConstClass` from a row. The broad case --
`Foo.class.isInstance(x)`, `LoggerFactory.getLogger(Foo.class)` wrappers, `synchronized
(Foo.class)` helpers spliced into callers -- needs: the resolver answering a row for any
loaded class in IR mode (extend the tuple to `(pc, holder, cp_idx, slot_addr, class_id)`),
`append_ir_inline_site` merging it into the builder's `ldc_class_info` / `ldc_slot_info`, and
the splice rules treating a `ConstClass` as what it is: a helper call that may raise
(resolution / access errors) -- the committed-store fence
(`IrBuilder::admits_after_committed_splice_store` refuses it today, correctly), the planner's
replay fence (`ir_splice_body_may_trap_after_a_call` must count it as a trap AND as a call),
and the chain snapshots. **Benefit.** Medium on framework code (class literals are common in
accessors). **Cost.** Medium. **Risk.** Medium: a `ConstClass` inside a splice deopting with
the wrong frame. **First step.** Count `inline-resolve REFUSED .. ldc-non-numeric-constant`
lines (named in wave 7) on the Spring and Tomcat censuses; split String from Class.

## S7-2. `holdsLock` identity through two `ldc` of one constant-pool entry

**What.** `IrBuilder::holds_lock_same_object` compares nodes, so `synchronized (C.class) {
Thread.holdsLock(C.class); }` (two `ldc` of the same entry build two `ConstClass` nodes) keeps
its call, and `ir_optimize::elide_nested_monitors` cannot see that `synchronized (C.class)`
nested in `synchronized (C.class)` is recursive. Two `ConstClass` of the same `(holder,
cp_idx)` are the same object whenever both produce a value (JVMS 5.4.3: one resolution per
entry). **Benefit.** Small (legacy code locking on class literals). **Cost.** Small: one arm
in `holds_lock_same_object` and in the monitor-object root used by the region passes.
**Risk.** Low. **First step.** A builder test with the region shape above.

## S7-3. A static method monitor seen through φs of class literals

**What.** RV6-1's `holdsLock(C.class)` fold matches only a `ConstClass` node itself;
`Class<?> c = C.class; while (..) { holdsLock(c) }` meets a loop φ. The finished-graph fold
(`ir_optimize::fold_method_monitor_holds_lock`) could strip trivial φs to a `ConstClass` of a
proven entry the same way it strips φs of the receiver (store the proven entries on the
`Graph`, not only on the builder). **Benefit.** Tiny. **Cost.** Small. **Risk.** Low.
**First step.** Only if S7-2 lands (same identity helper).

## S7-4. Name the remaining unnamed refusals in `resolve_inline_site_from`

**What.** Wave 7 named the `ldc` refusal; the `ldc2_w` loop's `_ => return None` and its `?`
on `constant_pool.get` are still silent, as are several `?` returns further down (field and
method refs). A census cannot count what it cannot see. **Benefit.** Diagnostic only.
**Cost.** A few lines in an interpreter-round file. **Risk.** None (the `no!` macro only
prints under `CRATONVM_DBG_JITC`). **First step.** `rg -n "return None|\)\?;" ` over the
function and convert each to `no!("<reason>")`.
