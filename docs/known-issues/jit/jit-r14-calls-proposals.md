# JIT round 14 wave 1, lane calls: proposals (call cost, megamorphic dispatch, MH doors)

Status: PROPOSALS (ideas for triage, not work items)
Area: `jit/src/ir_lower.rs` (residency plan, `Op::Call` routes), `jit/src/runtime_lowering.rs` (hashed stub), `native-builtins/src/lang_invoke.rs` (MH direct lane), `vm/src/jit/helpers.rs` (MH carrier site)
Found by: round 14 wave 1 lane calls

Context: this wave landed CC5-1 (`CRATONVM_JIT_IR_RESIDENCY_CALL_CROSSING`, see
`r12w8-callcost-fib-call-anatomy-20260927.md`), turned M9-1 into a script
(`C:\craton\jitr14-probes\m91-mega-anatomy.sh`) and answered the MH direct-lane question from the
round-13 battery (`r12w7-mh5-...`, round-14 section). Ranked by expected value per unit of risk.

## C14-1. A decoded per-handle record for the MH direct lane (MEDIUM, MEDIUM)

* **Benefit.** The direct lane's floor is ~2.2-2.6 us a call (w12a: `static-int` 77 ms,
  `exact-1` 78 ms per 30 000). Every call re-derives facts that are pure functions of an immutable
  handle: `mh_read_desc`, `mh_read_class`, `mh_read_name` (three by-name field reads and three Java
  `String` -> Rust `String` decodes), `split_descriptor_params`, and `mh_declared_descriptor` (a
  by-name `type` read plus a mirror-to-name render per parameter), before comparing the result to
  the call site. A record keyed by the handle's identity makes `mh_direct_lane_shape` one lookup
  and one descriptor compare.
* **Cost.** A per-VM map from the handle's weak lock key (the `VhMetaTable` pattern already in
  `lang_invoke.rs`) to `(kind, class, name, desc, params, own_type, declared_is_own_type)`, filled
  on the lane's first admission; the call site compare stays per call. Invalidation: none needed
  for the decoded strings (the fields are final), but `MH_BOUND` and the marking bits must still
  be read per call (or proven final for the carrier).
* **Risk.** Medium: a stale record on a handle whose `type` field is re-stamped (`asType` copies
  are new objects, so only an in-place stamp would be a hazard -- check `mh_with_stamped_type`).
* **First step.** Count per call, under a debug flag, how many of the three decodes the lane does
  (it is always three today) and time `R13Irexc2MhFloor exact-0` with the lane's shape read
  twice per call, to price the reads.

## C14-2. Constant method handles in compiled code (HIGH for MH-heavy code, LARGE)

* **Benefit.** The only route to HotSpot's ~0 per call: `static final MethodHandle H = ...;
  (int) H.invokeExact(x)` becomes a guarded direct call to the leaf in the optimizing tier. Every
  JDK-internal and framework `static final` handle (VarHandle-free `MethodHandle` code, `Lookup`
  based dispatch in Spring / Jackson) takes this shape.
* **Cost.** Large. In the IR planner: a `getstatic` of a `static final` field of an initialized
  class, feeding `invokevirtual MethodHandle.invokeExact/invoke`, whose current value is the VM's
  carrier with a direct-lane shape (C14-1's record) at the site's exact descriptor. Emit `CMP
  [field], imm64(handle)` + `JNE .generic` + the leaf's static / virtual call (its own IC). The
  handle identity must be a GC-stable reference: a constant-pool style root owned by the artifact,
  updated by the collector, or a guard on a per-handle id word instead of the address.
* **Risk.** Medium-high: object identity in code, class redefinition of the leaf's class (the
  ordinary dependency record covers the leaf method), and exceptions thrown by the leaf must look
  as if they came through the door (they do: the door adds no frame of its own on the direct lane).
* **First step.** A census of `invokeExact` / `invoke` sites whose receiver is a `getstatic` of a
  `static final` field, on the Spring sample and the R13 battery (how many sites, how hot).

## C14-3. Call-crossing residency for the other `Op::Call` routes (LOW-MEDIUM, MEDIUM)

* **Benefit.** CC5-1 admits a call RESULT only on the direct self-call route, because only that
  route writes its result through one `store_rax` (`home_is_one_store_rax`). `int a = f(x); int
  b = g(x); return a + b;` with `f` an IC or cross-call keeps its home store and reload.
* **Cost.** An audit of each `Op::Call` lowering route (`emit_direct_cross_call`, the inline-cache
  cascade, the hashed-stub edge, the dispatch helper) for "the result is written once, from RAX,
  at one site, and every other exit leaves through the call-exception stub", then admit those
  routes in `home_is_one_store_rax` (which also opens their home drop for two-use values).
* **Risk.** Medium: a route with two publish sites (e.g. a hit path and a helper path joining at
  `.done` with separate stores) would drop a home one path wrote. `every_droppable_op_writes_its_home_once_through_store_rax`
  is the test to extend.
* **First step.** `CRATONVM_DBG_IR_LINEAR_SCAN=1` on `R13Callcost4CallChains`: `single_use=`
  against `call_crossing=` per compile.

## C14-4. A counted variant of the hashed stub, debug-only (LOW, MEDIUM)

* **Benefit.** If M9-1's arms and samples (`m91-mega-anatomy.sh`) disagree about which lookup tier
  serves, a per-tier hit count answers it directly: PIC way k, per-site hashed way, class cell,
  shared table, helper.
* **Cost.** Under a debug switch read at compile time only, an `ADD qword [abs], 1` in each hit
  tail of `emit_hashed_vtable_stub_body` / `emit_mega_class_slot_probe` /
  `emit_mega_dispatch_table_probe` and in both cascades' way hits, into a per-site counter block
  appended to `JitPICSlot` (a tail field: no generated-code offset moves), printed by the
  `mic-prof` dump. Byte-identical code when off.
* **Risk.** Low (debug-only), but it perturbs what it measures, which is why it is the fallback.
* **First step.** Only if step 3 of the script leaves the attribution ambiguous.

## C14-5. Flip `CRATONVM_JIT_SP_IC_PROTECTED_SITES` (M9-2) -- the runs, not code (MEDIUM, LOW)

Restated because no round-13 or round-14 run has the switch ON: `R13Mega9ProtectedSites` in the
eight battery arms with `CRATONVM_JIT_SP_IC_PROTECTED_SITES=1` (must match HotSpot and the off
arm), then the Spring census with it on, then the flip. Every call at a `try { visitor.visit(n) }
catch` site in a precise-frame method is the Rust helper until then.
