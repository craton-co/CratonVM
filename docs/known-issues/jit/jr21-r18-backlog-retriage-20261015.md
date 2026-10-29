# Round-18 JIT backlog re-triaged against the round-21 tree

> **j27 wave 8 (lane W8UPC, 2026-10-28): PARTLY, PENDING HOST VERIFICATION; no new code.** The section-2 work list lives in `j27-r18-backlog-live-rows-20261028.md`; this wave swept it for code-only rows and found two (MON17-3, SC-4), marked there NOT BUILT with the reason and a census recipe. Every remaining live row is `HOST`. Retire this page once the rows j27 marked as done in code (the live-rows page's "The j27 rows done in code" section and the inline `FIXED IN CODE` marks) are host-verified; nothing here needs another code pass first.

> **j27 wave 3 (lane BACKLOG, 2026-10-28): PARTLY FIXED IN CODE, PENDING HOST VERIFICATION; superseded as the work list by `j27-r18-backlog-live-rows-20261028.md`.** Every section-2 row was re-checked on the j27 tree (git grep of its ID, switch and function names; flag rows in `types/src/flag_groups.rs`; `docs/internal/` pages of rounds 21-27). 24 of the 219 rows changed state since round 21's triage and are marked inline (`j27 (lane BACKLOG): ...`): rows DONE / OBSOLETE by round-21 lanes that never updated this page, rows built and awaiting host measurement, six rows done here (STR16-P7, TV2-5, CN16-3 in code; SDR17-3, MH3-8, MH517-3 stale comments retired). The 197 rows still live are copied, one line each with owner area and what is needed, into `docs/known-issues/jit/j27-r18-backlog-live-rows-20261028.md`; once the host confirms the j27 rows below, this page can be retired to `docs/internal/` (sections 3-5 are history: section 3's other-owner rows were not re-triaged here). Verify recipe: build the merged tree, run `cargo test -p cratonvm-jit --lib j27_backlog_sync_chain_gate_tests`, `cargo test -p cratonvm-vm --lib j27_backlog_decimal_digits_tests read_thread_task_object`, then `tools/probes/jit/j27/J27DecimalDigitsRows`, `J27ThreadTaskRows`, `J27SyncChainRows` (the last with `CRATONVM_JIT_IR_SYNC_METHOD_CHAINS` unset and =1) against HotSpot 25: every `fact` line equal.


Status: OPEN (the live proposal list for the next JIT rounds; section 2 is the work list)
Area: JIT backlog (the 88 C2, 261 C3 and 69 other-owner rows of `docs/internal/fixed-bugs/jit-backlog-triage-r18-CLOSED-20261012.md` section 3), plus the r18/r19/r20 `*-proposals.md` books
Found by: JIT round 21 wave 16, lane triage18 (docs only; read against `claude/jit-r21-round` at b0688e51eb; handoff `r21-HANDOFF-next-jit-round-20261015.md` section 6 item 5)

Re-land (20261026): merge `624fae0b49` had dropped this round-21 code from dev; it is back on dev since `44f0382076` (formatted 3-way re-land, `docs/internal/jit/jr21-merge-624fae0b49-dropped-round-21-code-20261026.md`). The status above describes the code again; its host verification is recorded there.

## How the verdicts were taken

Each row was checked on this tree: `git grep` of the ID and of the functions / switches it names, its flag row in `types/src/flag_groups.rs` (`off_word: Some("0")` = default ON, `None` = opt-in), the family defaults in `types/src/hotspot_alignment.rs`, `git log -S` / `--grep`, and the r19 / r20 / r21 summary records and `docs/internal/`. One to three greps per row: a verdict marked `(unverified)` means the grep found no sign of the work, not that the code was read through. Re-check those before building.

* **DONE**: built and default ON, or the decision the row asked for was taken. **DONE-OFF** (counted under DONE) means built behind an opt-in switch: the switch queue decides it, nothing is left to build.
* **OBSOLETE**: the premise no longer holds. Usually a HotSpot-alignment family now runs the JDK bodies, or the native was retired, or a later item replaced it.
* **STILL-OPEN**: JIT-lane work still worth doing. Value is h/m/l (high/med/low).
* **OTHER-OWNER**: still open, but not JIT work. "(untracked)" means no live page under `docs/known-issues/` carries the row.

Changes of owner: nine perf-owner rows were moved to STILL-OPEN, because `docs/known-issues/perf/` no longer exists and all nine are JIT codegen work (R16B-024, -025, -042, -086, -089, -097, -100, OD16-2, STR16-P2). R16B-045 became OBSOLETE: its levers were measured and rejected. R15B-43 is the 69th other-owner row (owner "owner"); round 18's section 3.4 lists it under the owner column.

## 1. Counts

| tier (r18) | rows | DONE (of which DONE-OFF) | OBSOLETE | STILL-OPEN | OTHER-OWNER |
|---|---|---|---|---|---|
| C2 worth building | 88 | 30 (9) | 1 | 43 | 14 |
| C3 after census | 261 | 22 (0) | 14 | 167 | 58 |
| other-owner | 69 | 19 (0) | 2 | 9 | 39 |
| **total** | **418** | **71 (9)** | **17** | **219** | **111** |

STILL-OPEN by value: high 7, medium 74, low 138. Of those, 55 are marked `(unverified)`.

### 1.1 Top 15 to propose first

These are the 7 high rows, plus the 8 medium rows with the clearest payoff (a measured gap, a HotSpot parity defect, or a cited blocker of other rows):

1. **STR16-P3** (h): the optimizing tier has no String access intrinsics. `string-intrinsic-pin` is still default ON. Run one A/B with `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1` on S17Kernel / R16Bench strings. This unblocks TP16-13 and TP16-16.
   *Round 21 lane strintr:* premise stale on 2a0ec6cbb6 -- the IR tier answers every String / CharSequence intrinsic member; the pin now pins only StringBuilder-site methods (`CRATONVM_JIT_IR_SB_PREFIX` opt-in). Two coder gaps closed (`CRATONVM_JIT_IR_STRING_COMPARE_TO_MIXED`, `..._INDEXOF_SUPPLEMENTARY`); the deciding A/B is in `jr21-strintr-string-pin-residual-20261015.md` section 3.
2. **FF3-3** (h): no JNI twin of the lazy in-native state. The FFM half (`CRATONVM_FFM_DOWNCALL_IN_NATIVE_LAZY`) is ON.
   *Round 21 lane jnilazy:* built inside the JNI in-native package (default off): `CRATONVM_JNI_IN_NATIVE_LAZY` (default ON with the package) takes a call with empty local frames into C through the FFM lazy state; page `jr21-jnilazy-jni-in-native-lazy-FIXED-20261027.md` (PENDING HOST VERIFICATION, probe `R21JnilazyRows`). Review fix: the lazy claim missed JNI local frames (`docs/internal/fixed-bugs/jr21-lazy-native-claim-misses-jni-local-frames-FIXED-20261027.md`).
3. **JO4-1** (h): one throwable builder in `native-api`, plus a ratchet on raw throwable mints.
   *Round 21 lane jnilazy:* `native-api/src/throwable_builder.rs` (`throw_new`, `construct_new`), both twins forward to it, ratchet `native-builtins/tests/throwable_mint_ratchet.rs` frozen exact; page `jr21-jnilazy-throwable-builder-and-ratchet-FIXED-20261027.md`.
4. **JNI6-1** (h): an in-process JNI conformance harness (a cargo test that drives the function table through a C shim).
   *Round 21 lane jniharness:* built without a C shim: `vm/src/native/jni_conformance_tests.rs` (fixture classes assembled in Rust, natives bound by `RegisterNatives`, the table driven through the native's `JNIEnv` slots, arms counted / deposit / lazy, two GC rows); review fix to `MonitorExit` / `MonitorEnter(NULL)`; page `jr21-jniharness-jni-conformance-harness-FIXED-20261027.md` (PENDING HOST VERIFICATION).
5. **MH6-3** (h): a compiled guard on the handle word for constant binds (unverified).
6. **MHB-3** (h): a compiled `bindTo` (unverified).
7. **P16-1** (h): `AnnotationInvocationHandler` in the handler slot. `annotation-carrier-is-handler` is still ON. Needs staging (unverified).
8. **BN17-4** (m): the BigInteger/BigDecimal arithmetic arm on the JDK bodies. BN17-2/3 won 5-40x with the same shape. Shown by the bignum battery's scenario D rows.
   *Round 21 lane bignum:* OBSOLETE on 5f2a7b61cf. The rows were deleted, not gated, so no `CRATONVM_BIGNUM_JDK_ARITH` exists; the JDK bodies run in both modes. Kernel review fix and `CRATONVM_BIGNUM_KERNEL_DIRECT` are in `jr21-bignum-jdk-arith-FIXED-20261027.md`.
9. **IN16-6** (m): IR lowering for record `hashCode` / `equals` (`ObjectMethods`): 768 ns here vs 2 ns on HotSpot.
10. **FG16-3** (m): `ir-splice-putstatic` still refuses. Round 21's alloc pages cite it. Shown by R21AllocRows p-field-n32 vs p-n32.
11. **OD16-2** (m, was perf): the OSR dataflow live-in admits primitives only, not references. Affects the `HashMap.putVal` / `treeify` headers.
12. **R15B-39** (m, parity): under `--compatible` the NPE message is not deferred (the `&& is_jdk_only()` guards in `exceptions.rs` / `invoke.rs`). Shown by `vm/tests/jit_npe_message_hot_equals_cold.rs` plus a `--compatible` arm.
13. **AO-1** (m, parity): HotSpot's `getDeclaredMethods()` order is applied to annotation classes only. Probe: a plain class compared against HotSpot 25, both modes.
14. **MEGA17-6** (m): the `perf annotate` reading of a megamorphic hit has never been taken. It decides `mega-vtable-column` (opt-in). Rows: R16MegaOneSite osr-virt24 / R17MegaVtable.
15. **OI-4** (m): `putstatic` still drops the static memo cell. No store-to-load forwarding (R20PLoopStaticCells).

Closed: **R16B-177** (FIXED f3bf482d29, diagnostic-only; no root defect) (gc owner, section 3). `verifier_local_verdict` / `classify_frame_slot` still read `java_locals_hi` as inclusive, but R16B-092 (`CRATONVM_JIT_IR_PUBLISH_LAST_LOCAL`, ON) publishes it as exclusive. This may be a real root-scan defect: the GC owner should check it first.

## 2. STILL-OPEN, sorted by value (the proposal list for the next rounds)

The source rows (`item`, `first step`) are in the r18 page's section 3 under the same ID. Within each value band, C2 rows come before C3 rows.

### 2.1 High (7)

| ID | r18 tier | what is missing | probe / row that shows it |
|---|---|---|---|
| FF3-3 | C2 | no JNI lazy in-native state (only ffm-downcall-in-native-lazy ON + jni-in-native-* doors) **j27 (lane BACKLOG): DONE** -- round 21 lane jnilazy, CRATONVM_JNI_IN_NATIVE_LAZY; docs/internal/fixed-bugs/jr21-jnilazy-jni-in-native-lazy-FIXED-20261027.md | - |
| JNI6-1 | C2 | no in-process JNI conformance harness **j27 (lane BACKLOG): DONE** -- round 21 lane jniharness, vm/src/native/jni_conformance_tests.rs; docs/internal/fixed-bugs/jr21-jniharness-jni-conformance-harness-FIXED-20261027.md | cargo test driving the JNI function table through a C shim |
| JO4-1 | C2 | no throwable builder in native-api and no ratchet in native-builtins/tests **j27 (lane BACKLOG): DONE** -- round 21 lane jnilazy, native-api/src/throwable_builder.rs + native-builtins/tests/throwable_mint_ratchet.rs; docs/internal/fixed-bugs/jr21-jnilazy-throwable-builder-and-ratchet-FIXED-20261027.md | ratchet counting raw throwable mints outside the builder |
| MH6-3 | C2 | no compiled handle-word guard for constant bind (jit-mh-compiled-link covers linker sites) (unverified) **j27 (lane BACKLOG): BUILT, PENDING HOST MEASUREMENT** -- jr21-mhbind-constant-bound-and-bindto-20261015.md (nothing left to write; the host row decides) | built: `jr21-mhbind-constant-bound-and-bindto-20261015.md` (PENDING HOST MEASUREMENT) |
| MHB-3 | C2 | no compiled bindTo (unverified) **j27 (lane BACKLOG): BUILT, PENDING HOST MEASUREMENT** -- jr21-mhbind-constant-bound-and-bindto-20261015.md | built: `jr21-mhbind-constant-bound-and-bindto-20261015.md` (PENDING HOST MEASUREMENT) |
| P16-1 | C2 | annotation-carrier-is-handler still ON; AnnotationInvocationHandler not in the handler slot (unverified) **j27 (lane BACKLOG): OBSOLETE** -- docs/internal/fixed-bugs/jr21-p161-annotation-handler-slot-OBSOLETE-20261027.md | - |
| STR16-P3 | C2 | `string-intrinsic-pin` still default ON (kill CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1); IR tier has no String access intrinsic; r19 strgap calls it the big lever; blocks TP16-13/16 **j27 (lane BACKLOG): OBSOLETE (premise stale)** -- the IR tier answers every String intrinsic member; the pin pins StringBuilder sites only; residual A/B in jr21-strintr-string-pin-residual-20261015.md | S17Kernel chunk pin A/B (=1 vs default), R16Bench strings |

### 2.2 Medium (74)

| ID | r18 tier | what is missing | probe / row that shows it |
|---|---|---|---|
| AO-1 | C2 | lang_class.rs still gates hotspot_declared_method_order on ACC_ANNOTATION only **j27 (lane BACKLOG): DONE** -- round 21 lane npecompat: lang_class.rs plain_class_method_order_is_exact / hotspot_method_array_order; docs/internal/fixed-bugs/jr21-npecompat-plain-class-declared-method-order-FIXED-20261027.md | getDeclaredMethods() order of a plain class vs HotSpot 25, both modes |
| BN17-4 | C2 | no CRATONVM_BIGNUM_JDK_ARITH; the arithmetic arm is not built (BN17-2/3 won 5-40x). r21 lane bignum: OBSOLETE on 5f2a7b61cf (rows deleted); see `jr21-bignum-jdk-arith-FIXED-20261027.md` **j27 (lane BACKLOG): OBSOLETE** -- rows deleted, JDK bodies run in both modes; docs/internal/fixed-bugs/jr21-bignum-jdk-arith-FIXED-20261027.md | r12w8 bignum battery scenario D add/multiply rows; R21BignumRows |
| CD16-1 | C2 | CRATONVM_COMPAT_NULL_ARG_MESSAGE_FROM_BYTECODE still OFF; literals not retired | - |
| CD16-4 | C2 | unmod_list_snapshot still walks a toArray() snapshot; only iterator() is live | unmodifiableList(l).listIterator(i) then l.add vs HotSpot |
| COL8-4 | C2 | cx6_1_null_argument_census still #[ignore] "asserts nothing" (native-collections) | - |
| FFM417-6 | C2 | no vm integration test of the STW-vs-blocked-downcall deadlock | 3-thread pipe read/write + forced GC, bounded time |
| FFMSEG17-2 | C2 | no skip-acquire for global/implicit/ofAddress sessions (unverified) | strlen-global / strlen-auto with CRATONVM_DBG_FFM acquire counts |
| M5-2 | C2 | no census of thin helpers bound without direct_bind_admission (unverified) | - |
| M5-5 | C2 | --compatible directInts 1.4 us never profiled (unverified) | R16 mega5 directInts row, --compatible |
| MEGA17-6 | C2 | perf annotate reading never taken; mega-vtable-column still opt-in | R16MegaOneSite osr-virt24 / R17MegaVtable virt-twentyfour perf |
| MH6-1 | C2 | converting bind still unboxes Integer only (mh_bind_unbox_int) | R20DispMhLinkRows + converting rows |
| MH6-2 | C2 | warm-path TLS probes unchanged (unverified) | - |
| MH7-4 | C2 | jit-mh-constant-compile handles exact direct static handles only | - |
| MHAPP-2 | C2 | type half ON (mh-loader-scoped-types); MH_CLASS dispatch / findConstructor / field finders open (unverified) | - |
| MHB-2 | C2 | only jit-mh-constant-bind-shape (MHF-1); no receiver-crossing shape bind (unverified) | - |
| MHF-3 | C2 | mh_constant_bind_redirect still heap.get_field per guard slot (helpers.rs) | - |
| MON2-1 | C2 | hires timer for timed Object.wait exists; contended-enter park/heir unverified on Windows | - |
| R15B-39 | C2 | NPE message deferral still jdk-only (exceptions.rs and invoke.rs `&& is_jdk_only()`); --compatible bridge reads detailMessage **j27 (lane BACKLOG): DONE** -- round 21 lane npecompat: the deferral serves --compatible too (native-builtins/src/lib.rs deferred_npe_message); docs/internal/fixed-bugs/jr21-npecompat-deferred-npe-message-both-modes-FIXED-20261027.md | vm/tests/jit_npe_message_hot_equals_cold.rs + a --compatible arm |
| RD16-1 | C2 | CachedInvokeTarget::Native (resolution.rs) has a RedefineGate but no owner | - |
| SBT16-P1 | C2 | lang_string.rs sb_append_object_through_bytecode / string_value_of_through_bytecode still do class_name_of_id + class_declares_method + by-name invoke per call | R16StringsInterpSb appendObj, CRATONVM_TOSTRING_UPCALL_FRAMES=1 vs =0 |
| TR16-1 | C2 | raise_through_bytecode has 5 call sites only; the census `top` sweep never done | native-builtins/tests/r16_trace_lost_frame_census.rs `top` rows |
| TR16-5 | C2 | no full-frame differential battery for common JDK raises | widen R16TraceCharAt to parseInt/ArrayList.get/Optional.get/CME, both modes |
| WMTE-1 | C2 | asType cast/unbox raises (reference_cast_failure, mh_unbox_for_primitive) still native: no Class.cast / ValueConversions frames (unverified) | MH asType CCE/NPE full-frame diff vs HotSpot |
| BUFCMP17-3 | C3 | no copySwapMemory0 / copyMemory0 direct helper in jit/src or vm/src/jit | R17BufCmp bulk; R20HuntFFMBulk |
| C2W2-2 | C3 | "neither spliced nor direct-bound" refusal remains in jit_bridge; no census | census of those refusals |
| CH16-1 | C3 | PublishedChainPoints (jit/src/deopt.rs) has points+kind only, no cold bit | CRATONVM_DEOPT_EAGER_CHAINS census on the R16 chain probe |
| CH16-5 | C3 | CRATONVM_DEOPT_EAGER_GUARDS not built | forced flat-guard exits on R20HuntDeopt* probes |
| CH2W3-2 | C3 | no instance-final trust for records / hidden classes; G14-1 census not taken | G14-1 scan census on hashmap kernels |
| CP16-3 | C3 | no OSR trip-count tier credit (unverified) | CRATONVM_TIER_OSR_BACKEDGE=100 on R16ColdpathHashMap P2 |
| CSR16-1 | C3 | dispatch_static.rs computes isolated_url_definition per call; no cached isolated-loader invokestatic arm (unverified) | R20HuntDispLoaders isolated-loader invokestatic row |
| CSR16-2 | C3 | is_isolated_url_loader_definition (constants.rs) recomputes per call, no memo (= FO4-1) | R20HuntDispLoaders |
| FG16-3 | C3 | jit_bridge `ir-splice-putstatic` still refuses; r21 alloc pages re-cite FG16-3. **Built (r21 wave 17, lane staticfwd, `CRATONVM_JIT_IR_SPLICE_PUTSTATIC`), PENDING HOST VERIFICATION: `jr21-static-store-forward-and-splice-putstatic-FIXED-20261029.md`; loop-bearing kernels (every R21AllocRows / R21Alloc2Split kernel) stay refused (`ir-splice-putstatic-loop`)** **j27 (lane BACKLOG): BUILT, PENDING HOST VERIFICATION** -- CRATONVM_JIT_IR_SPLICE_PUTSTATIC, jr21-static-store-forward-and-splice-putstatic-FIXED-20261029.md; loop-bearing kernels stay refused | R21StaticfwdRows *-splice rows; R21AllocRows p-field-n32 vs p-n32 / R21Alloc2Split p-n1 |
| FG16-4 | C3 | no receiver-residency change for field-dense bodies (unverified) | field-dense getter loop, reloads in DBG_JITC disasm |
| FO5-4 | C3 | CRATONVM_JIT_FIELD_PEEK_NO_LOAD still opt-in; no census | census of withheld fills and constraint refusals |
| FS16-5 | C3 | run-time per-arm inline-field counter not built (compile side only) (unverified) | R16Fieldsite2Layout census |
| FS3-3 | C3 | JUL Logger synthetic layout still in class_manager.rs (unverified) | R16Fieldsite2Layout --compatible logger row |
| IN16-6 | C3 | no record hashCode/equals (ObjectMethods) IR lowering; 768 ns vs HotSpot 2 **j27 (lane BACKLOG): BUILT, PENDING HOST MEASUREMENT** -- jr21-records-record-objectmethods-fold-20261015.md | record hashCode/equals row -- built: `jr21-records-record-objectmethods-fold-20261015.md` (PENDING HOST MEASUREMENT) |
| MG16-2 | C3 | census line mic_cell_budget_refused exists (helpers.rs); Spring census not run | Spring run, mic_cell_budget_refused |
| MG16-5 | C3 | ~70 ns entry-vs-OSR call gap unmeasured | same large direct call, entry vs OSR body, perf |
| MH16-7 | C3 | no bound-static admission (unverified) | R16MhW8Bind bound-static row |
| MH16-9 | C3 | moving mock still opt-in per test (unverified; test-only) | - |
| MHAPP-1 | C3 | user-loader bound-type rows not admitted; measure first (unverified) | R16 mhapp app-arm row |
| MHB-4 | C3 | batch slot read for guard words not built; needs perf first | perf of MH-bound handle rows (guard-word reads per call) |
| MON17-2 | C3 | no coarsened_enters census word | [monitor-inline census] thread-end line on R19Mon10Rows |
| MON17-5 | C3 | block-coarsening admission (ir_optimize.rs) still Load/Store/LoadStatic/StoreStatic/Guard only; no array ops | R16MonitorStaticLoop; DBG_JITC "a body node could call, allocate or resume elsewhere" |
| MON17W2-2 | C3 | no cross-block null proof for non-receiver field-monitor bases (unverified) | R17Monitor2StaticFinal + `synchronized (holder.lock)` phase |
| MON3-4 | C3 | single-pass monitor still flushes registers eagerly (unverified) | R20HuntDeoptMonitor / R20FuzzCMonitorDeopt1 |
| MON4-2 | C3 | gate met (BLOCKS ON since r17); no census or code for a block calling a spliced leaf | census: block refusals with a spliced-leaf call |
| OI-4 | C3 | putstatic still `StaticMemoEffect::DropCell` (jit/src/ir.rs); no static store-to-load forwarding. **Built (r21 wave 17, lane staticfwd, `CRATONVM_JIT_IR_STATIC_STORE_FORWARD`), PENDING HOST VERIFICATION: `jr21-static-store-forward-and-splice-putstatic-FIXED-20261029.md`** **j27 (lane BACKLOG): BUILT, PENDING HOST VERIFICATION** -- CRATONVM_JIT_IR_STATIC_STORE_FORWARD, jr21-static-store-forward-and-splice-putstatic-FIXED-20261029.md | R21StaticfwdRows put-get-*; R20PLoopStaticCells / R20PLateStaticCell |
| R15B-03 | C3 | OSR_HANDLER_EXIT_FALLBACK ON since r16 but no LOCAL_HANDLER_* measurement since (unverified) | R15Trace6OsrImplicitLines both modes (probe must be recreated) |
| R15B-22 | C3 | CRATONVM_JIT_INVALIDATION_RETIRES_SUPERSEDED still opt-in; no census | define-heavy workload timing the jit_entry_owners walk |
| R15B-33 | C3 | only ic_hit_exits/ic_admissible census print (ir_lower.rs); no out-of-line IcRegion tail | ir-call-cold-tails census on a mono-virtual kernel |
| SD17-6 | C3 | ir_self_lock_candidate still refuses every exception table | CRATONVM_DBG=jit-method-stats self-lock census ref-table rows |
| SDB17-3 | C3 | shared sentinel exits still refused (body-ref-handler-exits row); no hand-over twin | jit-method-stats body-ref-handler-exits, server battery |
| SDB17-4 | C3 | jit_local_handler_lookup still maps LOCAL_HANDLER_DECLINED (-2) to -1 before the stub | R17SyncDoor te/rc shapes |
| SDB17-5 | C3 | no JVMTI exception-event arm touching compiled_catch_events_armed | R17SyncDoor tc + JVMTI agent arming Exception events |
| SDR17-1 | C3 | CalleeHandlerMiss has no Unresolved variant | DBG_DEOPT refuse_callee_rerun line |
| STR17-6 | C3 | -XX:-CompactStrings still not honoured (COMPACT_STRINGS preset true) | R17StringsSb vs HotSpot -XX:-CompactStrings |
| SY16-1 | C3 | no lock-free entry when the caller already holds the monitor; no census | R20FuzzCSyncExc1 (guard); census of caller-held sync calls |
| SY16-6 | C3 | `sync-direct REFUSED ... calls=` log still in jit_bridge.rs; census not run | count `calls=true` refusals on the battery |
| TF15-3 | C3 | not built; waits on perf showing the stub | - |
| TP16-13 | C3 | blocked on STR16-P3: `string-intrinsic-pin` still default ON **j27 (lane BACKLOG): LIVE CODE+HOST** -- no longer blocked on the pin (jr21-strintr-string-pin-residual-20261015.md); a String-intrinsic null-receiver row of its own | R20FuzzCIntrinsicNpe1 |
| TP16-16 | C3 | blocked on STR16-P3 pin; no IR implicit-NPE node for it **j27 (lane BACKLOG): LIVE CODE+HOST** -- no longer blocked on the pin (jr21-strintr-string-pin-residual-20261015.md); the IR implicit-NPE node is the open part | R20FuzzCIntrinsicNpe1 / R20FuzzENullTrap1 |
| TP16-24 | C3 | partly: SPLICE20-2 (b0141f4af4) covers unloaded `new` in splices only; general compiled-body trap+recompile not built | R20HuntLinkNestUnloaded |
| TP16-5 | C3 | no SIOOBE-without-deopt path; STR20-1 (0f7bb010eb) keeps charAt as a call | R20StrCharAtLicence |
| TP16-7 | C3 | premise changed (TP16-6 capture memo now default ON); raw signature not built | R21WalkthrowFacts / R21Walkthrow2Rows |
| TR16-2 | C3 | frame_is_vm_bridge still keys on JIT_INDY_BRIDGE_FRAME_CLASS name (stackwalker.rs); no vm_synthetic bit | - |
| TR16-4 | C3 | TR15-5 census not ranked; needs a Spring start-up | - |
| VH1-2 | C3 | CRATONVM_VH_INDIRECT_TYPED still opt-in; 26 arrayElementVarHandle bridges remain | FFM VarHandle probe arms, VH_INDIRECT_TYPED=1 |
| VH8-1 | C3 | VarHandleJudgedSite has no exact-handle verdict memo | exact-handle library loop A/B |
| OD16-2 | OO | OSR live-in by dataflow admits primitives only (CRATONVM_JIT_IR_OSR_DATAFLOW_LIVE_IN, opt-in); references not admitted (was OTHER-OWNER perf) **j27 (lane BACKLOG): DONE** -- CRATONVM_JIT_IR_OSR_DATAFLOW_LIVE_IN_REFS default ON, host-verified 2026-10-26; docs/internal/fixed-bugs/jr21-osr-dataflow-live-in-references-FIXED-20261026.md | HashMap.putVal / treeify OSR headers |
| R16B-024 | OO | straight-line register allocation still spills (CG16P); docs/known-issues/perf no longer exists, so this is JIT-lane work (was OTHER-OWNER perf) | R16 codegen kernels, DBG_JIT_DISASM spill count |
| R16B-086 | OO | matrix j loop spills both element loads and FP intermediates (CG16P-3 regalloc) (was OTHER-OWNER perf) | R16MatrixKernel.mm |
| R16B-089 | OO | splicing a loop-bearing callee loses the loop's own profile-guarded virtual splices (CB16-1) (was OTHER-OWNER perf) | callbind kernel |

### 2.3 Low (138)

| ID | r18 tier | what is missing | probe / row that shows it |
|---|---|---|---|
| BN17-6 | C2 | bd_alloc/bi_alloc/bd_write_into still in math_bignum.rs (10 bd_alloc callers) | - |
| CB17-6 | C2 | table-only intrinsics still unpublished in the census | --dump-native-registry diff |
| EINTR17-2 | C2 | measurement only (xt-takeover-skip-in-syscall default ON); no D vs D0 record | GceE2tTakeoverProbe D vs D0, medians of 5 |
| MH16-11 | C2 | no G1/ZGC/--compatible constant-bind battery arm (unverified) | - |
| MH217-6 | C2 | strict helper-path loader refusals never measured | fat-jar LaunchedURLClassLoader bind census |
| MHAPP-3 | C2 | three native MethodType mints not interned (i3 mh-compat-method-type-bytecode may cover part) (unverified) | - |
| MHEDITOR17-1 | C2 | only mh_is_real_handle (MH_KIND test) exists; no single "real JDK handle" predicate (unverified) | - |
| MHEDITOR17-2 | C2 | no MH_KIND_DOWNCALL; downcall carrier still kindless | - |
| P16-2 | C2 | TypeNotPresentExceptionProxy type-name carry not built; sentinel message (unverified) **j27 (lane BACKLOG): OBSOLETE (default configuration)** -- the JDK parser builds the proxy itself; open only in the carrier arm; jr21-p161-annotation-handler-slot-OBSOLETE-20261027.md | - |
| RD16-5 | C2 | no receiver-keyed slot-4 refusal (unverified) | - |
| SBEMPTY17-2 | C2 | no audit of natives copying a literal or `this` (unverified) | - |
| SBEMPTY17-4 | C2 | string_literal_identity_enabled still a process OnceLock (lang_string.rs; op_invoke.rs) -- AGENTS.md per-VM rule | - |
| TF17-2 | C2 | retained frames still carry a per-frame label Arc (unverified) | throwable-vary-depth-512 perf |
| AL16-3 | C3 | no IR-tier fresh-unstored SATB skip (single-pass ctor-store-from-null only) | - |
| AL16-5 | C3 | spliced_putfield_receiver_is_local0 (x64/op_field.rs) still aload_0-only; census not run | - |
| BN17-7 | C3 | 10^n per MathContext not cached (unverified) | - |
| BUFCMP17-4 | C3 | one off-heap classification helper not built (unverified) | - |
| C2W2-6 | C3 | callee_compiler_bound (jit_bridge.rs) keeps clinit-gap re-check + long note | CRATONVM_DBG_JITC |
| CB17-5 | C3 | cleanup not done (unverified) | - |
| CH16-3 | C3 | CRATONVM_DEOPT_EAGER_CHAINS_SEED not built | seeded eager-chains run, diff forced sets |
| CH6W5-2 | C3 | no HS-2 nested exact-row list (unverified) | - |
| CN16-3 | C3 | `sync_method_chains_enabled() && !ir_self_lock_requested` still spelled at three sites in jit/src/lib.rs (hygiene) **j27 (lane BACKLOG): FIXED IN CODE, PENDING HOST VERIFICATION** -- j27 wave 3: one per-compile binding ir_sync_chains_ok = deopt::ir_method_admits_chains(..) in ir_tier_attempt replaces the three spellings; unit test j27_backlog_sync_chain_gate_tests; probe J27SyncChainRows | - |
| CSR16-3 | C3 | no JitInvokeInfo constructor; ~230 struct literal sites (mechanical) | - |
| CW2-3 | C3 | COMPAT_STREAM_TOLIST_UNMODIFIABLE ON, no A/B recorded (unverified) | Stream.toList loop =0 vs default |
| FFC16-3 | C3 | handle CIF never freed (panama.rs "when finalizer path lands" note) | create/drop many downcall handles, watch RSS |
| FFD16-5 | C3 | dead DowncallHandle arms still in invoke.rs and vm/src/jit/helpers.rs (dead-code retire) | - |
| FFM16-4 | C3 | panama_upcall.rs still builds shape.method_descriptor() String per upcall | R14FfmUpcallDirect |
| FFM217-3 | C3 | segment-call slot trust not built; is_object_address walk still used (unverified) | R20HuntFFMSegBounds |
| FFM217-5 | C3 | no real-session arm; MMapDirectory decline count not taken | - |
| FFM417-4 | C3 | no decision on critical(true) with a heap capture-state segment (unverified) | R20HuntFFMHeapTear |
| FFM417-5 | C3 | no active_downcall_region().is_open() masked decision | - |
| FFMSEG17-6 | C3 | mutex-free memo verdict not built (unverified) | R21NativeFfmDoorExactOnly |
| FFR2-1 | C3 | p67_implicit_register_cleanup still builds CompletableFuture/UniAccept (foreign_ffm.rs) | allocation count of R13Ffm7AutoArenaCleanup.drop |
| FO4-1 | C3 | = CSR16-2: url_classloader_isolated_from_app recomputed per call, no per-loader memo | - |
| FO4-6 | C3 | no ResolvedFieldSite type; CompileRequest still has separate closures | - |
| FS14-2 | C3 | VM still refuses chain virtuals (deopt_resume chain_virtuals_refusal) | sync method chain naming a virtual, SPLICE_CHAIN_VIRTUALS=1 |
| FS14-3 | C3 | no positive-control probe for the sync chain-virtual disagreement | same, fail-without-fix |
| FS3-2 | C3 | class_manager.rs still asserts int/long atomic arrays keep length at slot 1 | R16Fieldsite2Layout |
| FS3-4 | C3 | allocate_log_manager still synthetic LM_NUM_FIELDS (unverified) | - |
| FS3-6 | C3 | FLOOR_EXEMPT_CLASSES still present (classloading lib.rs, class_manager.rs) | - |
| IN16-3 | C3 | walk_invokedynamic still calls blind emit_pre_safepoint_spill() (op_invoke.rs) | - |
| IN16-4 | C3 | indy_site_is_bridged still requires ret_type != 'V' (op_invoke.rs) | Groovy/Kotlin void-indy census |
| IR10-2 | C3 | single-pass walk_invokedynamic still bridges constant sites (unverified) | CRATONVM_DBG_JITC [indy-constant] count |
| IR10-4 | C3 | r21 TIER_RETRY_OWED (8668331736) covers AlreadyQueued only, not the c2_bailout refusal | indy-constant retry REFUSED census |
| IR10-5 | C3 | sole_indy_bci_of still used (jit_bridge.rs); OSR doors ask by CP index | - |
| IR2-5 | C3 | ir_scalar_intrinsics_enabled still a process OnceLock (jit/src/ir.rs) | process_global_statics_ratchet |
| IRFEED17-4 | C3 | no test asserts the feed's filter on Graph::trusted_final_static_fields | unit test: try_compile_request with final / non-final / refused-base statics |
| IRLOWER17-2 | C3 | CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES still opt-in; memory phis still wants_loc/wants_slot; A/B first | `[ir-ls] peak_live` under CRATONVM_DBG_IR_COMPILES |
| IRLOWER17-3 | C3 | ir_lower.rs bci_native_dominated recorded but never counted; ir_verify does not refuse undominated anchors | undominated-anchor count under CRATONVM_DBG_IR_COMPILES |
| IRLOWER17-4 | C3 | ir_lower.rs transfer_bcis still a HashSet<usize> keyed by bci, not snapshot | refused compiles of copied bodies under DBG_JITC |
| IRLOWER17-5 | C3 | no Graph::copy_of; by-bci readers still infer copies | - |
| M5-4 | C3 | protected-site residue census not run | shape-1 protected-site census |
| MEGA17-4 | C3 | column fill on callee publish not built; mic-prof warm-up census not run | R17MegaVtable virt-fortyeight first 100 ms vs steady, CRATONVM_DBG_MIC_PROF |
| MEGA17-5 | C3 | census not run (CRATONVM_DBG_SP_IC_SITES exists) | CratonBench + Spring with DBG_SP_IC_SITES=1 |
| MEGA2-17-3 | C3 | probe phase missing (version-52 private method through invokevirtual) | new R17Mega2 phase, version-52 classes |
| MEGA3-17-3 | C3 | no publishable() on CachedDispatchTarget; condition still written out in helpers.rs | - |
| MG16-8 | C3 | blind site still calls table.peek_interned_way per miss, no selector memo | perf peek_interned_way on a blind-heavy run |
| MH16-10 | C3 | mock does not forward the valueOf table (unverified; test-only) | - |
| MH16-4 | C3 | populate_method_type_form still allocates two SoftReference[] caches per type | - |
| MH16-5 | C3 | no bound-null sentinel; bindTo(null) adapter path (unverified) | - |
| MH217-4 | C3 | gated: CRATONVM_JIT_MH_CONSTANT_INTERP_FILL and _FIELD_ROWS still opt-in; no measured need | R17MhconstCompile exact-static-gc, DBG_JIT_MH_BIND |
| MH217-5 | C3 | MH_COMPILED_BIND_SHAPE_SLOTS still includes MH_STATIC_SETTLED (lang_invoke.rs) | R16MhFloor bind-each, DBG_JIT_MH_BIND fills per site |
| MH3-8 | C3 | stale "never consulted by dispatch" note in lang_invoke.rs **j27 (lane BACKLOG): STALE DOC RETIRED** -- j27 wave 3: the note moved to native-builtins/src/lang_invoke/call_site.rs and was wrong (MH_MARKS_DIVERT_DIRECT_LANE and the field links read MH_VARARGS); rewritten | - |
| MH317-4 | C3 | mh_constant_bind_loaders_agree unchanged; Spring door-verdict count not taken (unverified) | Spring fat-jar DBG_JIT_MH_BIND verdicts |
| MH417-3 | C3 | gated on CRATONVM_JIT_MH_CONSTANT_FIELD_ROWS (opt-in); no wanted-witness set | R17MhconstCompile exact-static-gc |
| MH517-3 | C3 | stale "cannot reproduce" hash comments remain (access_control.rs, exceptions.rs) **j27 (lane BACKLOG): STALE DOC RETIRED** -- j27 wave 3: classloading/src/access_control.rs builtin_loader_name_and_id and vm/src/runtime/exceptions.rs loader_name_and_id docs now name the user-loader nameAndId readers (r17 mh5) | - |
| MH6-4 | C3 | gated on MH6-1 census; not built | - |
| MH6-5 | C3 | per-site converting-bind census line not built | - |
| MH7-5 | C3 | MH7 single-pass tier not built (unverified) | - |
| MH717-1 | C3 | "handle's method type .. but found" formatted at three sites in lang_invoke.rs, no helper | - |
| MH717-2 | C3 | spoofed-member ICCE for real classes not built | jasm outer class whose InnerClasses omits its nested class |
| MHAPP-4 | C3 | gated on app arm > 1785 ns; not measured | R16 mhapp app-arm row |
| MHB-5 | C3 | no `--flagreads` arm for handle rows in the probe kit | probe-kit `--flagreads` arm over handle rows |
| MON17-3 | C3 | not built; only with a wider takeover-region change | - |
| MON4-5 | C3 | not built; waits on a profile | - |
| MON5-5 | C3 | refusal "the monitor's getfield base varies in the loop" still in jit/src/ir_optimize.rs; not counted | count "base varies" refusals |
| MW13-3 | C3 | receiver-not-dominant census not run | receiver-not-dominant census |
| MW13-5 | C3 | gate (rnd > 1.5x poly3) not measured | mega2 rnd vs poly3 rows |
| NIO2-17-6 | C3 | buffer_session_class_is_served + served-class table still present (jit/src/direct_helpers.rs) | --dump-native-registry --compatible: session() on receivers outside java/nio |
| P16-3 | C3 | not measured; needs a Spring boot host **j27 (lane BACKLOG): OBSOLETE (premise)** -- the default path returns the handler slot; the cost belongs to the carrier arm; jr21-p161-annotation-handler-slot-OBSOLETE-20261027.md | Spring boot getInvocationHandler count/time |
| R15B-13 | C3 | panama.rs budget counts auto-arena allocations only; shared-close bytes not fed (FF2W3-2) | 256 x 64 MiB shared-close RSS probe |
| R15B-14 | C3 | FFM_SHARED_RECORD_PER_THREAD exists (r13) but FF2W3-3 one-way CAS not built | unit test: two tags on one session |
| R15B-21 | C3 | lang_invoke.rs still get_field_by_name(vh,"componentType") per store; VH5-2 memo not built | AtomicReferenceArray store loop A/B |
| R15B-23 | C3 | no per-cache weak list (VH7-3); multi-VM only | multi-VM probe with JIT_VH_JUDGED_BINDS=0 |
| R15B-24 | C3 | varhandle_ir_refusal_protects_bind and varhandle_splice_keeps_thin_bind still separate (VH6-5) | - |
| R15B-40 | C3 | DBG_RTERR path in exceptions.rs still formats eagerly (unverified) | - |
| R16B-015 | C3 | exec_sampler.rs still drops native-call time | CRATONVM_PROFILE_SAMPLE_MS on the HashMap.newTreeNode kernel |
| R16B-031 | C3 | SB6 only partly answered; perf record not run | R16SyncSyncM p, INLINE_PRIM_PUTFIELD=0 |
| R16B-037 | C3 | valueof-full 184 vs 14 ns still unexplained | R16OsrdoorParts valueof-full |
| R16B-060 | C3 | jit_note_external_alloc (helpers.rs) still fetch_add + TLS probe per call | Object[8192] anewarray under ZGC |
| R16B-091 | C3 | varhandle_compare_and_exchange_raw still by-name field + set_field per unresolved call | - |
| R16B-111 | C3 | CRATONVM_TRIVIAL_GETTER_FIELD_SITE still cached_is_set! (env_cache.rs) | - |
| R16B-113 | C3 | no enqueue dedupe near bc-callee-tiered-enqueue (lambda.rs) (unverified) | 24-receiver threshold burst |
| R16B-123 | C3 | concat toString throw propagates but no String.valueOf/stringOf frames synthesized | compiled concat with throwing toString, trace diff vs HotSpot |
| R16B-129 | C3 | bindTo insert-adapter pin release on alloc failure not verified (unverified) | - |
| R16B-138 | C3 | class_id=64 coercion not explained in any doc | R16StringsInterpSb --compatible + CRATONVM_DBG_LAYOUT=1 |
| R16B-139 | C3 | sb_raise_through_builder_body has only charAt/deleteCharAt/setCharAt callers | - |
| R16B-182 | C3 | native_method_handle_invoke still hands the downcall carrier straight to pe_downcall_invoke | - |
| R16B-183 | C3 | invokeExact door downcall null-arg NPE text unverified | - |
| R16B-198 | C3 | no current-binary recheck of Re.area acceptance; ACCEPT21-1 census switch now exists | CRATONVM_DBG_ACCEPT_CENSUS on polyclear (Re.area) |
| RD16-2 | C3 | no owner-scoped hit counter (WK4-4) in code | - |
| RD16-4 | C3 | inherited_declaring_class still returns None for interface defaults (dispatch_virtual.rs) | redefine a class inheriting an interface default |
| SA6-3 | C3 | has_trapping_indy_site (x64/op_invoke.rs) still spells indy_site_is_bridged, not crate::indy_rows_have_trap | r15w6_sweepa_indy_table_refusal_tests |
| SBT16-P2 | C3 | builder charAt rows under --compatible still differ (unverified) | SB/SBuf charAt rows, --compatible |
| SC-1 | C3 | no coarsening census or code | - |
| SC-4 | C3 | not built | - |
| SC-5 | C3 | not built; census not run | - |
| SDR17-2 | C3 | no per-reason census of resolve_callee_uncached None | DBG_DEOPT exit census |
| SDR17-3 | C3 | implicit_throw_is_uncovered doc still says "synchronized" blocks resolution (stale doc) **j27 (lane BACKLOG): STALE DOC RETIRED** -- j27 wave 3: implicit_throw_is_uncovered's doc (vm/src/jit/helpers/thread.rs) now says a synchronized callee resolves under CRATONVM_JIT_SELF_LOCKING_SYNC_HANDLERS | - |
| SELECT17-2 | C3 | no `tail-supertype` dispatch-tally label | Tomcat --compatible, CRATONVM_DBG_DISPATCH_TALLY |
| SELECT17-5 | C3 | no unit test calls receiver_selection_retarget | two-loader ClassStore unit test |
| STC17-2 | C3 | no per-thread high-water mark next to deopt_stash_nesting_counts (deopt.rs) | DBG_DEOPT exit line |
| STD17-4 | C3 | only aggregate drop-released/unreleased rows (self_lock_census.rs); no per-site counters | CRATONVM_DBG=jit-method-stats per-site split |
| STD17-5 | C3 | OSR keep rule still in place (unverified) | - |
| STR16-P6 | C3 | the two inline array emitters are not folded (unverified) | - |
| STR16-P7 | C3 | per-call DECIMAL_DIGITS_CENSUS fetch_adds still in jit_decimal_digits_latin1_direct_body (vm/src/jit/helpers.rs) **j27 (lane BACKLOG): FIXED IN CODE, PENDING HOST VERIFICATION** -- j27 wave 3: the per-call served/declined fetch_adds (read by nothing) removed from jit_decimal_digits_latin1_direct_body (vm/src/jit/helpers/fast_native.rs); DECIMAL_DIGITS_CENSUS keeps the two compile-time site slots; text pin j27_backlog_decimal_digits_tests; probe J27DecimalDigitsRows | Integer.toString hot loop perf |
| STR17-3 | C3 | jdk_only_sb_bytecode_enabled re-reads env each call (not latched) | --jdk-only startup, CRATONVM_DBG_FLAGREADS=1 |
| SY16-3 | C3 | no caller-held pairs recorded on the graph | - |
| SY3-6 | C3 | door_locked_wrapped_body still enters only requires_wrapped_entry bodies | disp_census OUT_STATIC_BC count |
| TF15-4 | C3 | make_not_entrant_as(bodies, by_redefinition: bool) still carries no reason (jit/src/not_entrant.rs) | - |
| TP16-19 | C3 | probe-kit item; holder-field probe-store change not found (unverified) | - |
| TP16-25 | C3 | eager callee chain policy unchanged, not measured (unverified) | - |
| TR16-3 | C3 | thread dump / JVMTI view of the `<jit-indy>.bridge` frame unchanged (unverified) | JVMTI GetStackTrace across an indy bridge |
| TR2W2-6 | C3 | for_name_rename_array_cnfe (lang_class.rs) has no backtrace copy / hint re-arm | R15Trace2LoaderMissShapes row |
| TR3W3-4 | C3 | no per-call-site name hash in the artifact side table; compiled callers still screen under class-manager lock (unverified) | lock-count census on a Spring request loop |
| TR4W3-6 | C3 | JBossThread.run frame "still missing (TR4W3-6)" (wildfly_core.rs comment) | - |
| TV2-5 | C3 | read_thread_task_object (vm_exec.rs) still uses resolve_field_index_in_hierarchy, not thread_field_slot **j27 (lane BACKLOG): FIXED IN CODE, PENDING HOST VERIFICATION** -- j27 wave 3: read_thread_task_object (vm/src/vm/vm_exec.rs) reads target / holder through thread_field_slot; unit test read_thread_task_object_reads_threads_own_fields_through_the_layout_memo; probe J27ThreadTaskRows | - |
| UP1-2 | C3 | register_upcall/get_upcall_info still in native-api/src/registry.rs + mocks | - |
| UP1-3 | C3 | panama_upcall.rs has DirectStatic only; no bound-virtual door | bound-virtual row in R15Upcall1Arity |
| WALK17-2 | C3 | capture_tail_trace still uses active_compiled_frames_bounded; no cut-aware widening | deep OSR'd compiled recursion + bounded tail walk |
| WMTE-2 | C3 | no `CollectorAsType` hint in code | - |
| WMTE-4 | C3 | not built; waits on perf | - |
| WMTE-5 | C3 | no door-throw frame census in the probe kit (unverified) | - |
| R16B-025 | OO | nocall loop IDIV / bounds residue not addressed beyond R16MegaOneSite virt rows (was OTHER-OWNER perf) | R16 nocall loop rows |
| R16B-042 | OO | emit_copy_op constant phi source (CG16P-4) not built (was OTHER-OWNER perf) | - |
| R16B-097 | OO | cold-arm coldness still shape-only (INLINE_COLD_ATHROW); branch counters unused (was OTHER-OWNER perf) | arraylist w7 kernel |
| R16B-100 | OO | spliced Atomic* unbox rows not built (was OTHER-OWNER perf) | callbind kernel |
| STR16-P2 | OO | no inline Unsafe.putByte / putShortUnaligned on byte[] at the array base (digit stores) (was OTHER-OWNER perf) | Integer.toString digit-store kernel |

## 3. OTHER-OWNER (still open, not JIT work)

| ID | owner, evidence, live page | value |
|---|---|---|
| R15B-29 | FFM owner (panama.rs): no stamped asType copy of the compact downcall (unverified) | l |
| R16B-196 | FFM ABI owner decision; no Linux check | l |
| FFR2-5 | FFM/per-VM: auto_arena_cell still one process OrderedPlMutex<HashMap> (panama.rs) | l |
| SC6-1 | NIO natives: nio_file.rs p57_windows_check_and_normalize still separate | l |
| SH15-2 | SBR: unconstructed_carrier_gate.rs unchanged (unverified; untracked) | l |
| SH15-3 | SBR: System$1 rows still in retired_shadow.rs; SBR register internal, SBR rounds closed at SBR5 (untracked) | l |
| SH16-3 | SBR: System.getProperty retired (RETIRED_SHADOW_SBR_SYSTEM_TRIPLES); getProperties/setProperty/clearProperty HELD on the VM property store | m |
| JO3-5 | SBR/jdk-only: no registry diff recorded | l |
| JO4-3 | SBR/jdk-only: unconstructed_carrier_gate.rs does not read alloc_single_message_exception funnels (unverified) | l |
| JO4-5 | SBR/jdk-only: Unsafe array-geometry shadows not retired (unverified) | l |
| JO5-4 | SBR/jdk-only gate: no readUTF codec gate test | l |
| JO5-6 | SBR/jdk-only ratchet: no discarded-constructor-result ratchet | l |
| JO6-5 | SBR/jdk-only gate: no Class-mirror instantiation funnel gate | l |
| JO6-7 | SBR/jdk-only: blocked on newInstanceWithCaller | l |
| JO7-4 | SBR/jdk-only: reflective_access_refusal exists but no gate counts its callers | l |
| JO7-5 | SBR/jdk-only: DataOutputStream natives still registered | l |
| R15B-41 | SBR/jdk-only: register_classloader_natives still registers the DataInputStream family | l |
| W3-2 | SBR/jdk-only: native-io InputStream.transferTo/readAllBytes still SyntheticStub | l |
| SH2-3 | SBR/native retirement: no per-image retirement dimension in retired_shadow.rs | m |
| CMC12-2 | SBR/native-builtins: ArrayList.iterator row still registered | l |
| CB17-2 | VM dispatch owners: one Compatible "bytecode wins" rule at the walk doors not built | h |
| MH517-2 | VM runtime/classloading: three loader-name readers remain (exceptions.rs, loader_constraints.rs, constants.rs) | l |
| TVM-2 | VM threads: ~22 by-name reads of eetop/tid/holder/interrupted/parkBlocker, no ratchet | l |
| VH8-5 | aarch64 (out of scope): aarch64-backend-runs-no-java-on-a-real-machine-20260922.md | - |
| CS-1 | classloading (interp L5): ISOLATED_LOADER_DICTIONARY still OFF; Spring census cannot run (host suite gone) | l |
| AO-4 | classloading/orchestrator: needs Spring suites; RuntimePackageId absent | l |
| CX6-3 | compat collections: copy_ctor_bulk_override still hard-coded | l |
| LC16-P2 | compat native retirement: --compatible DecimalFormatSymbols.initialize native still registered (locale_resources.rs) | m |
| CMC12-1 | compat/SBR native retirement: nine ArrayList$ListItr natives still registered | m |
| R16B-187 | embedding: libcratonvm non-creating-thread contract (untracked) | l |
| AL16-2 | gc: one announce per TLAB word needs the linear-span contract (untracked) | l |
| CHN15-1 | gc: shadow-stack range format (untracked) | l |
| EINTR2-3 | gc take-over round: XT_PEERS_UNCLASSIFIED count not run | l |
| FF2W3-1 | gc: collection-epilogue FFM row sweep (untracked) | l |
| FFM417-2 | gc: gc_and_alloc.rs still reads the env each take-over round | l |
| FFMSEG17-5 | gc: in-place heap segments for critical(true) need a moving-collector list (untracked) | l |
| MON16-4 | gc: blocking-native deposit cost under the JIT (untracked) | l |
| R15B-12 | gc: CRATONVM_FFM_SHARED_ARENA_CLOSE_FREES still OFF; FF2W3-8 only in the out-of-scope tier-b page | l |
| R16B-009 | gc: -Xlog:gc young/ZGC cycle lines (untracked) | l |
| R16B-059 | gc: vm_tlab.rs reads per refill; tracked in known-issues/gc/zgc-zr4rv4-latched-construction-seeds-20261012.md | l |
| R16B-061 | gc: GenGC Object[16384] 3-4x HotSpot unattributed (untracked) | m |
| R16B-096 | gc: ZGC concurrent start only at helper/refill sites (untracked) | l |
| R16B-140 | gc: set_static_shared now fires satb_barrier on both paths -> premise may be gone; remark confirmation pending | l |
| R16B-142 | gc: gen_heap.rs write_barrier doc still says natives call satb_barrier themselves (untracked) | l |
| R16B-177 | gc: verifier_local_verdict / classify_frame_slot read java_locals_hi as inclusive while R16B-092 made it exclusive -- FIXED f3bf482d29, dropped by the dev merge 624fae0b49, reapplied on dev 2026-10-26 (diagnostic-only mismatch; `docs/internal/gc/jr21-java-locals-hi-convention-FIXED-20261026.md`) | m |
| R16B-179 | gc: rq_remove_wait sleeps in a loop under --compatible (untracked) | l |
| SBZGC17-6 | gc: proof to drop G1's same-region check (untracked) | l |
| TR6W6-2 | gc: conservative_roots.rs OSR pairing on the snapshot (untracked) | l |
| EINTR2-2 | gc/threads: only the debug guard CRATONVM_DBG_NATIVE_ALLOC_NO_GC_POINT exists; per-thread bit not built | l |
| C3W3-4 | interp L4: debug pin-balance census live (vm_exec.rs); Leaked not an assertion yet | l |
| IN16-2 | interp: site-trap attribution in resume_or_despeculate_stash_inner | m |
| LD6-1 | interp L5: initiating_records.rs records resolution/forName/define (i1 w38); supertype records unverified | m |
| LD6-2 | interp L5 loader work (one-resolved-class-entry-table proposal); no single define-namespace function | m |
| MEGA2-17-4 | interp: S111r12 fallback still copied in invoke.rs and helpers.rs; census not run | l |
| OD16-3 | interp: gate met (`ir-chain-arm` ON) but CR15-2 recompile not built; interpreter-round file | m |
| R15B-02 | interp: virtual_thread_group answers null before Thread$Constants init (untracked) | l |
| R15B-26 | interp (MH JDK-text fidelity, lang_invoke.rs): no HotSpot wording pass | l |
| R15B-30 | interp: lang_invoke.rs WMTE "cannot build collector" still has no cause | l |
| R15B-34 | interp (VarHandle NPE text): no change | l |
| R15B-37 | interp: create_exception_object takes several class_manager reads (untracked) | l |
| R15B-38 | interp: tracked by known-issues/interpreter/i98-L9-compatible-implicit-npe-text-is-a-detail-message-20261130.md | m |
| R15B-45 | interp: finalizable registration point JO6-2 (untracked) | l |
| R15B-52 | interp (serialization/JNI): no change (unverified) | l |
| R15B-53 | interp (reflection on lambda proxies): no constructor arm | l |
| R15B-55 | interp L5: cache_loader_initiated still last-wins; first-wins initiating_records.rs exists | l |
| R15B-60 | interp: no reference-array asType cast found after L4 setter fix | l |
| R16B-032 | interp: Constructor root sharing (untracked) | l |
| R16B-040 | interp: special tail runs the inherited native (untracked) | l |
| R16B-126 | interp: Code clone fixed; per-call class.name.to_string() remains in vm_exec.rs (untracked) | l |
| R15B-44 | interp/jdk-only: JO7-6 covered DataOutputStream only; no general census | l |
| JO2-5 | interp/stackwalk fidelity: no top-frame comparison | l |
| R15B-04 | interp/threads: ServedThreadRunCalls.incomplete still sticky (untracked) | l |
| R15B-05 | interp/threads: start0 context-loader copy census (untracked) | l |
| CB17-4 | interpreter: no link-time recognizer for JDK String.length/isEmpty; CRATONVM_STRICT_STRING_TABLE_BYTECODE opt-in (unverified) | m |
| MON17-4 | interpreter/monitor round: no contended_since in monitor.rs/vm_exec.rs | m |
| R16B-171 | io: set_field_by_name(this,"colon") still unconditional | l |
| CP16-4 | jdk-only native retirement: forced getGenericInterfaces still in native_override.rs, jit_bridge.rs, lib.rs | m |
| JO2-6 | jdk-only/SBR: p57_* / raf_utf_data_format flat fallbacks still in nio_file.rs; audit not done | l |
| LC16-P1 | locale-native/SBR: CLDR_PARENT_LOCALES still copied (locale_resources.rs) | l |
| R16B-164 | monitor/threading: WaitReacquirePark has no reservation | l |
| CP16-5 | native retirement: getGeneric* forced entries still in three copies, no resolve_dispatch routing | m |
| R16B-172 | native security/FFM: format!("loadLibrary.{target}") before the cheap return (security_manager.rs) | l |
| SH2-4 | native retirement: QName still minted synthetically (xml_stax.rs try_alloc_concurrent_synthetic) | l |
| SH2-5 | native retirement: kind-map copies unchanged (unverified; handoff 6.1 kind-map re-freeze) | l |
| BD14-3 | native-builtins math_bignum: precision store outside doRound unverified | l |
| CX6-5 | native-builtins compat: native_properties_put_all generic-Map fallback still present | l |
| R16B-068 | native-builtins strings: native_string_hash_code UTF16 path still get_array_element per byte | l |
| R16B-070 | native-builtins: Integer.toString(I) still registered (STR16-7-gated compat fn) | l |
| R16B-081 | native-builtins lang_system: is_abstract_string_builder_*_copy stack captures on every cross-type copy | l |
| C4W4-3 | native-collections compat: ad_add_all_by_iterator still walks the iterator; no NativeConsumer carrier | l |
| C5W4-3 | native-collections compat: pq_sift_up/down still swap-based compare order, not JDK hole-move | l |
| R16B-193 | native-collections: native_al_itr_has_next still cursor < size | l |
| JO7-1 | natives (jdk-only/interp): no instantiability() query | l |
| FFR2-6 | orchestrator probe kit (R13Ffm7AutoArenaCleanup) (untracked) | l |
| LD6-5 | orchestrator probe-kit battery (untracked) | l |
| MH517-4 | orchestrator: probe-kit masking for run-dependent hashes (untracked) | l |
| PX8-4 | orchestrator probe-kit battery (untracked) | l |
| R15B-01 | orchestrator (flag tables): flag-inventory rows of THROWABLE_THREAD_RUN_FRAME / STACK_WALK_THREAD_RUN_FRAMES / STANDIN_VIRTUAL_AS_BOUND still generic | l |
| R16B-120 | orchestrator: probe artefact R16MonitorCrowd (untracked) | l |
| R16B-163 | orchestrator: R16Monitor2Fairness for worst-wait A/Bs (untracked) | l |
| R16B-181 | orchestrator: throwpath-sampler.py kit tooling (untracked) | l |
| VH7-4 | orchestrator: UNJUDGED ratchet in the probe harness (untracked) | l |
| WK4-1 | orchestrator probe kit (untracked) | l |
| R15B-15 | owner decision: CRATONVM_COMPAT_IMMUTABLE_OF_JDK_MINT still OFF; CRATONVM_COMPAT_JDK_IMMUTABLES ON | m |
| R15B-43 | owner decision (kept SecurityManager vs JEP 486); partly met by CRATONVM_ACCESS_CONTROLLER_JDK_BODIES (interp L5, FIXED-20261014); SBR held register retired (untracked) | l |
| R15B-63 | owner decision (virtual-thread reference); CRATONVM_THROWABLE_STANDIN_VIRTUAL_AS_BOUND still OFF, one reader; trace6 page rewrite pending | l |
| R16B-165 | owner decision (locale): emulate HotSpot setlocale fallback, undecided | l |
| STREAMS3-3 | streams: StreamSupport.stream still registered; see STREAMS5-18-1 | m |
| STREAMS3-2 | streams/SBR: java/util/stream interface bridges not retired (unverified) | m |
| R16B-192 | types/error: IllegalStateException { message: String } has no null-message form | l |
| R16B-075 | vm_init locale: frozen ISO-639 mapping pinned by test; needs a he-IL Windows HotSpot reference | l |

## 4. DONE and OBSOLETE (evidence)

**DONE (62)**

* CH6W5-1 (C2): CRATONVM_JIT_IR_NESTED_GUARDED_SPLICE default ON since r19
* FF3-1 (OO): compiled native deposits built (current43); ffm3 page FIXED-20261012
* FF3-2 (OO): CRATONVM_FFM_LAZY_CLAIM_NO_ROOTS_PAUSE default ON + FFMEINTR18-1
* FF3-5 (OO): FF3-5 guard repair recorded in ffm3 page, FIXED-20261012
* FFL16-4 (C3): c0d06f7985: native_access_enabled() loads atomic NATIVE_ACCESS_ANY_GRANTED, no read lock
* FFL16-5 (C2): r19 direct_stub_call_fp float/double direct stubs (panama_libffi.rs, panama.rs); SysV only
* FFM217-2 (C2): CRATONVM_JIT_FFM_DIRECT_DOOR default ON (helpers.rs; jr21-native-ffm-bound-downcall-front page, FIXED 2026-10-26)
* FG16-5 (C2): CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE default ON (r19 osr2); r20 ir-loop-static-promote ON
* FS3-5 (C3): 55856ac8bb (cf2): real CompletableFuture skips the floor (CRATONVM_COMPAT_JDK_COMPLETABLE_FUTURE ON)
* FUNNEL17-4 (C2): CRATONVM_DBG_FUNNEL_INTRINSIC_CENSUS, vm/src/vm/native_funnel_census.rs (r19 funnel2)
* IR10-1 (C2): 71d33902e8 (r21 mcs) indy-mutable-guard-splice default ON
* IR10-3 (C2): 801d077bcd ir-splice-indy (ON, nio21 w11) + ir-splice-indy-capture (ON, r20)
* IRLOWER17-1 (C2): opt-in CRATONVM_JIT_IR_LICM_DIRECT_STATIC; same hoist default ON via ir-loop-static-hoist (+after-promote, a07a180361)
* JO4-2 (C3): THROWABLE_CTOR_ERRORS_PROPAGATE default ON
* MH2-1 (OO): memos read gc_address_epoch (5199845e21, e352e17ae0); page FIXED-20261012 in docs/internal/gc
* MH417-4 (C3): mh_invoke_return_cast_target casts ARRAY returns to the caller's array class (r18 w9 mhloader)
* MH517-1 (C2): access_control.rs field/method_access_denied_message_naming names user loaders (round 20, R20HuntLinkAccess)
* NIO-6 (C3): 8ba44987e1 (nio19): transferFrom0 via copy_file_range, native-io/src/file_channel.rs
* NIO17-2 (C3): Op::KeepAlive, CRATONVM_JIT_IR_REACHABILITY_FENCE_KEEPALIVE default ON (nio20 w3, NIO19-9)
* NIO2-17-3 (C2): NIO19-2: CRATONVM_JIT_UNSAFE_ACCESSOR_INLINE default ON (nio20 L2)
* NIO2-17-4 (C3): ScopedMemoryAccess.{get,put}Char* registered (r19 nio closed table)
* NIO2-3 (C3): 8ba44987e1 (nio19): abstract CharBuffer rows reclassified (retired_shadow.rs "closed here")
* NIO2-4 (C3): native-api/src/retired_shadow.rs: NIO2-3/NIO2-4 closed (nio19, 8ba44987e1)
* NIO3-17-2 (C2): JdkNioBuffers drops forced bulk stand-ins under --compatible (jit-r19-nio closed table)
* NIO3-17-3 (C2): s2_bb_equals answers false for a non-ByteBuffer first (servlet.rs)
* R15B-16 (OO): --compatible CHM runs JDK bytecode (jdk-concurrent-hash-map default ON, hotspot_alignment.rs)
* R15B-17 (OO): jdk-tree-map alignment default ON: --compatible TreeMap runs JDK bodies
* R15B-18 (OO): retired CHM rows yield to JDK bytecode via jdk-concurrent-hash-map
* R15B-19 (C3): 5edcb9e0b5 value check before coordinate cast; 27e18688d0 (r21 vhmerge) judges the handle's component
* R15B-54 (OO): raw get_field(loader_obj, 6) replaced (deprecated_internal.rs, L1)
* R16B-013 (OO): CRATONVM_JIT_OSR_WAIT_CREDITS_LOOP_WORK default ON (r20 w11)
* R16B-019 (C3): CRATONVM_MONITOR_WAIT_JFR_PRECHECK default ON (monitor.rs)
* R16B-038 (OO): page FIXED (docs/internal/fixed-bugs/r16-codegen-hashmap-get-anatomy-FIXED-20261005.md); residual not re-measured
* R16B-044 (C3): census answered: PARTIAL_UNROLL fired, acceptance gate discarded it (r16-codegen-loop-anatomy-FIXED item 5)
* R16B-057 (OO): Code clone in execute removed (ExecuteCode); page FIXED-20261006
* R16B-066 (C3): STRGAP19-6 CRATONVM_JIT_STRING_HASH_IS_ZERO default ON (bfa2e25136)
* R16B-071 (C3): overlay_cldr_bundle uses load_cldr_table_scripted (LC-B1, CRATONVM_CLDR_LOCALE_FIDELITY)
* R16B-083 (OO): page docs/internal/performance/r16-arraylist-kernel-is-19x-hotspot-FIXED-20261006.md
* R16B-084 (C3): IR graph listing built whole, one eprintln (jit/src/lib.rs "backlog MX-B1")
* R16B-085 (OO): MX-B2: index_zx_at skips MOV ECX,ECX after a zero-extending index load (ir_lower.rs)
* R16B-092 (OO): CRATONVM_JIT_IR_PUBLISH_LAST_LOCAL default ON (java_locals_hi exclusive)
* R16B-095 (C3): CRATONVM_JIT_IR_INLINE_TLAB_NEWARRAY default ON (Generational/G1)
* R16B-098 (OO): AL16-2 iterator SR: CRATONVM_JIT_SPLICE_CHAIN_VIRTUALS ON (ea-loop-carried FIXED-20261006)
* R16B-148 (C3): DBG_LOCAL_HANDLER_LOOKUP latched once (nio21 w17) and per VM (TP16-15)
* R16B-161 (OO): StringBuilder in FLOOR_EXEMPT_CLASSES; CRATONVM_STRING_BUILDER_COMPACT_LAYOUT default ON
* R16B-174 (C2): ffm-library-lookup-restricted-warn default ON (r18 w11 backlogfix)
* R16B-180 (C3): CRATONVM_JIT_IR_INLINE_PUTSTATIC_REF default ON (r20 w17)
* R16B-194 (C2): r18-listitr-remove-receivers-not-repinned-FIXED; test native-builtins/tests/r18_backlogfix_listitr_pins.rs
* SD17-5 (C3): decision: SYNCDOOR3-4 (SD17-5) REJECTED r19 w7
* SDB17-2 (C2): jit/src/self_lock_census.rs leak row (r19 selflock19); self-locking-sync-handlers default ON
* SDC17-4 (C3): monitors-held-at-thread-exit census row (SELFLOCK19-3, 0d8f6ecce3)
* STR16-P5 (OO): jdk-string-builder-bodies default ON: --compatible builders run JDK bodies
* STR17-4 (C3): String rows reviewed interpreter intrinsics; strict Bridge rows 38->0 (dee9baa92c)
* STREAMS3-4 (OO): BASELINE_INTRINSICS refrozen r20 (c32812224b); bridge-ratchet strict row 38->0 (dee9baa92c)
* TF17-4 (C2): decision: THROWS19-4 REJECTED round 19
* TF17-6 (C2): CRATONVM_THROWABLE_PLAIN_CAP_BUILD default ON (stackwalker.rs)
* TF17-7 (C2): decision: THROWS19-5 REJECTED round 19 (~5%, below the A/B floor)
* TP16-15 (C2): CRATONVM_JIT_LOCAL_HANDLER_LEAN default ON (r19 throws19)
* TP16-21 (C2): CRATONVM_JIT_LATE_BIND_SELF_LOCK default ON (r19 eager19, jit_bridge.rs waiter)
* TVM-6 (C3): JMX/management enumerate_threads(usize::MAX); registered_thread_name removed (c34c1d03ea or 29c082e75a)
* WALK17-3 (C2): CRATONVM_STACKWALK_LEAN_CAPTURE (opt-in); step 2 rejected r19 walk7; WALKTHROW21-1 default ON 8498f1fa08
* WALK17-5 (OO): docs/internal/gc/r17-gc-compiled-chain-walks-stop-at-4096-activations-silently-FIXED-20261005.md

**DONE-OFF (9)**

* CH16-4 (C2): CRATONVM_JIT_IR_CHAIN_SNAPSHOTS_LAZY (r19 splice19) + DBG chain-snapshot census, opt-in
* IN16-1 (C2): CRATONVM_JIT_IR_STRING_VALUE_OF_CALL (r19 irops19), opt-in
* NIO2-17-5 (C2): class_facts_memo rungs CRATONVM_CLASS_FACTS_MEMO_STRING/WRAPPER/MIRROR/CARRIERS/VALUE_HASH opt-in; PROXY default ON (r19 funnel2/3)
* OD16-5 (C2): ir-call-site-exception-edges (nio21 w3-5) + r19 ir-compiled-handler-entry, all opt-in
* R15B-31 (C2): CRATONVM_JIT_IR_FP_MATH_OPS (r19 irops19: sqrt / FP min-max / multiplyHigh ScalarOps), opt-in, never flipped
* TF15-1 (C2): = SPLICE2-2: CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW (r19 splice3)
* TP16-22 (C2): CRATONVM_JIT_LATE_BIND_UPGRADE_DOOR (r19 eager19); OSR half CRATONVM_JIT_LATE_BIND_OSR (EAGER19-4), opt-in
* TP16-23 (C2): CRATONVM_JIT_SUPERSEDE_FORWARD_EVERY_DOOR (r19 eager19)
* WALK17-4 (C2): CRATONVM_COMPILED_WALK_ACTIVATION_MEMO (r19 throws2, not flipped)

**OBSOLETE (17)**

* C2-3 (C3): jdk-tree-map alignment default ON (55f08fadbe): TreeMap runs JDK bodies under --compatible
* C2-5 (C3): jdk-copy-on-write-set and jdk-streams ON; COW set uses the JDK spliterator (UTF-16 codePoints half unverified)
* CC13-2 (C3): TreeMap/TreeSet JDK bodies via jdk-tree-map ON (55f08fadbe)
* CC13-4 (C3): jdk-linked-collections ON (55f08fadbe): ArrayDeque/LinkedList run JDK bodies
* CD16-2 (C3): TreeSet (jdk-tree-map) and HashSet (jdk-hash-collections) run JDK bodies under --compatible (unverified)
* LC16-P3 (C3): jdk-locale-providers alignment ON: getDecimalFormatSymbolsData retired to the JDK body
* MHEDITOR17-3 (OO): SBR3-HANDOFF page CLOSED (superseded by SBR4, closed by SBR5); no SBR round left
* NIO17-3 (C3): reachabilityFence native row retired (RETIRED_SHADOW_SBR2_SMALL_TRIPLES); IR uses KeepAlive
* NIO17-4 (C3): name-guessing isDirect/isReadOnly rows retired as abstract-declaration rows (r19 nio)
* NIO3-17-4 (C2): moot under JdkNioBuffers: no forced buffer row left over a JDK receiver
* R15B-46 (C3): superseded by CRATONVM_REFLECT_CLASS_ACCESS (default ON, interp i2 w59)
* R16B-045 (OO): CG16P-5 levers were measured and rejected (was OTHER-OWNER perf)
* R16B-112 (C3): blind-mega OSR page CLOSED r20 (SPLICE20-3 residual, decline stop-gap ON); ACCEPT21-1 census (8668331736)
* R16B-170 (C3): EINTR->0 matches the JDK C poller; not Java-visible (native-io doc)
* SBEMPTY17-3 (C3): real-JDK builders run JDK bodies in both modes (JdkStringBuilderBodies ON)
* SBZGC17-7 (C3): --compatible real-image builders run JDK bodies (JdkStringBuilderBodies ON), which keep maybeLatin1
* STC17-1 (C3): SELF_LOCKING_SYNC_HANDLERS and SELF_LOCK_EXC_HANDOVER now default ON

## 5. The r18 / r19 / r20 proposal books

Each book's items were checked by the same method: inline LANDED / DONE / REJECTED marks, flag rows, ID cites in code, and the r19-r21 records. Note that the nio20 and nio21 books reuse `W*L*-n` IDs, so a code cite of such an ID was judged by the switch or the code it names. The orchestrator owns the moves; this lane moved nothing.

### 5.1 Fully DONE / OBSOLETE: retire to `docs/internal/jit-proposals/` (43)

* r18 (5): `jit-r18-mega5`, `jit-r18-osrframe`, `jit-r18-osrpoly`, `jit-r18-syncdoor6`, `jit-r18-throws`.
* r19 (26): `concat5`, `eager19`, `eager2`, `eager3`, `funnel3`, `handler11` (HANDLER12-1 obsolete: its page retired FIXED-20261012), `handler5`, `irops3`, `mega2`, `mega5`, `megaosr`, `nio`, `osr19`, `osr4`, `osr5`, `osrentry6`, `review6`, `selflock19`, `splice19`, `splice2`, `strgap`, `strgap4`, `syncdoor3`, `syncdoor4`, `throws19`, `walk11` (its leftover memo A/B arms are on `docs/internal/fixed-bugs/jr21-stackwalker-deep-walk-cost-split-FIXED-20261026.md` (retired 2026-10-26: `amemo` / `chains` measured flat, both stay OFF)).
* r20 (12): `jit-r20-loopopt` (LOOPOPT20-7 decided by `jr21-predicate-switch-ab-bimodality-20261014.md`), `jit-r20-nio-proposals-L4`, `-L5`, `-w2-L1`, `-w2-L2`, `-w2-L4` (L4-4's residual moved to NIO20W3-L5-3), `-w3-L2`, `-w3-L4`, `-w4-L2`, `-w5-L2` (W5-L2-2 moved to `jit-r21-nio-proposals-w1-L3`), `jit-r20-stash`, `jit-r20-wave20-residual` (REFL21-1 moved to `jit-r21-depth`; ADDR21-1 is the owner decision on `jr21-conf-segment-address`).

FFMCOST18-6 and FFMRET18-1/-4 were superseded into MEM20-6 (`jit-r20-memory`).

### 5.2 Still holding open items: keep (38)

* `jit-r18-backlogfix`: BACKLOGFIX18-3 (`NATIVE_ACCESS_POLICY` is process-global in `panama.rs`; it should be per VM, m).
* `jit-r18-ffmcost`: FFMCOST18-7 (drop the redundant NativeRunning span in the lazy non-critical transition, m).
* `jit-r18-ffmeintr`: FFMEINTR18-2 (route wait / monitor / join / IO waits through `run_blocked_wait`, m), FFMEINTR18-3 (l).
* `jit-r18-ffmret`: FFMRET18-2 (bounded ADDRESS return side slot, m), FFMRET18-3, FFMRET18-5.
* `jit-r18-mhloader`: MHLOADER18-3 (one memoised nest-host query), MHLOADER18-5.
* `jit-r18-mhresid`: MHRESID18-2 (cache a failed outer/enclosing resolution, JVMS 5.4.3).
* `jit-r18-streams5`: STREAMS5-18-1 (`--compatible` stream sources as real JDK pipelines, h), -4, -5, -6.
* `jit-r19-lv11`: LV12-2 (one flag flip after CI is green). `jit-r19-throws5`: THROWS5-2 (a measurement). Both are one low item each and could fold into the handoff.
* `jit-r20-atomics`: AT-1 (inline `memoryAccessWarned` for `sun.misc.Unsafe` accessors: ufcas 362 vs 4 ns, h), AT-6, AT-8, AT-9.
* `jit-r20-dispatch`: DISP20-4, -5, -6 (bound invokeExact after a child-first loader: 2.1 us vs 28 ns), -8 (CHM putVal 14x: inline load/CAS of a ref-array element), -9.
* `jit-r20-memory`: MEM20-5 (= L1-1 = L3W2-1, the chain rethrow pad), MEM20-6 (FFM per-field access: 769 vs 43 ns), MEM20-7, MEM20-9.
* `jit-r20-splice`: SPLICE20-3 (optimizing OSR body at blind megamorphic sites), SPLICE20-5. `jit-r20-strings`: STR20-3, STR20-5 (= W6-L2-6). `jit-r20-throw`: THROW20-2, THROW20-3. `jit-r20-walkcompat`: WALKCOMPAT20-1..6. `jit-r20-zCp`: ZCP-2.
* nio20 books with open items:
  * `L1` (L1-1, L1-3), `L2` (L2-1), `L3` (NIO20-L3-2);
  * `w2-L3` (L3W2-1, -2, -5), `w2-L5` (W2L5-1, -3, -5, -6);
  * `w3-L1` (W3L1-3: a jdk-nio-files family row for ~300 stand-ins, h; W3L1-4), `w3-L3` (W3L3-3, -4), `w3-L5` (NIO20W3-L5-1, -3, -4);
  * `w4-L1` (W4L1-2), `w4-L3` (W4L3-5, -7), `w4-L4` (W4L4-1, -3, -4: measure TLS cost, h), `w4-L5` (W4-L5-4, -5);
  * `w5-L1` (W5L1-4), `w5-L3` (W5L3-1), `w5-L4` (W5L4-1..4), `w5-L5` (NIO20W5-L5-1..3);
  * `w6-L1` (W6L1-4: a JIT-readable Unsafe arena, h), `w6-L2` (W6-L2-1, -3, -4, -5, -6), `w6-L3` (L3W6-3, -4), `w6-L4` (W6L4-3), `w6-L5` (NIO20-W6L5-5).

Fold candidates: several kept books hold only one or two low items: r19 `lv11` / `throws5`, nio20 `L2`, `L3`, `w4-L1`, `w5-L1`, `w5-L3`, `w6-L4`, `w6-L5`, and `zCp`. Merging them into one carry-over book would retire ten more files.
