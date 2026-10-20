# ZGC round zt: wave t3 (+ t4/ch) review residuals (lane t4/x3)

> **STATUS (2026-10-19, zt t6/rv3): FIXED IN CODE (items 1-5, zt2x1 item 5 two of three), PENDING BUILD/PROBE; 3's cost half, 6, 7 and the JFR wiring test are named rows.** 1 (`8c35eab1b1`): `dispatch_memo_name_epoch_enabled` reads through a `MemoSlot`; the narrow flush body is `narrow_dispatch_memo_flush`, tested one epoch at a time (`zt6rv3_a_definition_only_move_drops_refusals_and_keeps_answers`: definition-only move keeps answers and `selection_error` targets, drops refusals; name-generation / edge / origin / redefine moves flush in full = positive control) and the override reaches the switch (`zt6rv3_the_name_epoch_switch_reaches_a_thread_override`); the doc and `gc-tuning.md` name the kept `selection_error` rows. Not exercised: which epochs a real define moves (the classloading tests own that). 2 (`a119760be6`): one span per flush, opened in the caller before the filter, charged by the hand-offs past the pool lookup (no closing read on the no-pool arm); deciding row MK6 of `t3-mk-report.md` re-read, kill control `CRATONVM_ZGC_MARK_CPU_ACCOUNTING=0` prints `satb_flush_ms=0`. 3 (`9564c6a5dc`): `Some(false)` counts as `gate_collects_joined=` (line pins updated); the `gate_refused` shared counter's cost is decided by row CH-2 read with and without the census (<= 1.02x CPU), not done. 4 (`8feae2fc3b`): the booked-uncommit test gains a fresh-heap pass with accounting on that must book `uncommit_ns > 0` on Linux. 5 (`26ca100316`): `quarantine_span_straddled` falls back to `huge_extent_containing`; test `zt6rv3_a_held_span_deep_inside_a_huge_array_is_straddled` (300 MiB into a forged 320 MiB array; out-of-table control reads unclaimed). 6 (`4ec679b7b5`): `Zt1MkSystemGcFresh` prints `closing-causes` on stderr; deciding row: it vs `java -XX:+UseZGC` under `CRATONVM_ZGC_CONC_START=20`, no code change until it differs. 7: unchanged, the gn audit's row as written. zt2x1 item 5 (`7a41229c6c`): wiring tests `zt6rv3_start_uncommitter_consults_the_windows_pause_only_switch` and `zt6rv3_the_memo_address_return_site_collects_off_zgc` (non-ZGC arm; the ZGC arm needs a ZGC context: t1/nd probe row); the JFR per-pause safepoint count needs a recording backend this lane cannot build by reading: deciding row = the t3/jf `jdk.SafepointBegin` count row (ZGC 3 per collection vs 1).

> **STATUS (2026-10-19, zt t5/pg3): OPEN.** Items 1-7 as written (`dispatch_memo_name_epoch_enabled` to a `MemoSlot` plus a test; `hand_thread_batch_to_the_marker`, gc/src/zgc.rs:8078, one span after the pool is found; `note_zgc_gate_refused`, gc_and_alloc.rs:10475, and a `gate_collects_joined=` field; a strictly positive uncommit test arm; `release_evac_quarantine`, gc/src/zgc.rs:13001, falling back to the huge-extent table; the closing collection's cause row; the GN-6 audit). Carried from `zgc-zt2x1-wave-t1-*` item 5: the three wiring tests it lists.

> **STATUS (2026-10-19, zt t4/x3): OPEN.** Review findings on `ac2059ae81..55ef6f552b` (wave t3 lanes jf, tp, nd, mk, rl, fp, ob, gn, x2, the orchestrator's merges, and t4/ch) that this lane did not fix: they need a host row, a test harness this lane cannot build by reading, or a maintainer decision. The fixes this lane made are in `docs/internal/zgc-round-zt-20261018/t4-x3-report.md`. Line numbers are on `zt4-x3`.

## 1. The narrow JIT dispatch-memo flush has no test, and cannot get one as written (nd, opt-in)

`flush_class_identity_dispatch_memos_narrow` (`vm/src/jit/helpers.rs:4787`) is behind `CRATONVM_JIT_DISPATCH_MEMO_NAME_EPOCH` (opt-in). By reading, no positive entry goes stale on a definition-epoch-only move:
- Every class definition that can change an existing answer moves one of the four keyed epochs:
  - a second definer of a name, a removal or an unload moves the name generation (`loaded_classes_insert` / `_remove`, `class_manager.rs:16429-16463`, `13404`);
  - a forward edge, `set_superclass` or `set_interfaces` moves the hierarchy-edge epoch (`class.rs:1673`);
  - a stub upgrade calls `set_origin`, which moves the in-place origin epoch (`class_manager.rs:4840`);
  - a redefinition sets the sticky flag.
- The refusal-like entries the narrow arm keeps are `VIRTUAL_TARGET_CACHE`'s `selection_error: true` rows (`helpers.rs:7036-7120`): unresolvable owner, receiver does not implement, strict selection error. Each is a function of the owner's or the receiver's stored hierarchy and method tables, which a brand-new class cannot change without moving an epoch.
- The one input outside the class store is `jit_site_unresolvable_owner`'s native-registry probe (`helpers.rs:35670`), which a lazy registration pass could change. The full arm does not cover it either.

What is missing:
- **No unit test.** There is none for either arm.
- **The switch cannot be driven by a test.** It is latched in a process `OnceLock` (`dispatch_memo_name_epoch_enabled`, `helpers.rs:4775`), so a thread override never reaches it.
- **The doc overstates.** It says "refusals dropped", but only the three bytecode-callee memos' `None` entries and `WRAPPED_STATIC_CALLEE` are dropped.

**Closes when:**
- the switch reads through a `MemoSlot`, as nd did for `CRATONVM_ZGC_FFM_MINT_COLLECT` (`panama.rs` `ffm_mint_collect_switch`);
- a test fills the memos for two sites, defines a brand-new class (only the definition epoch moves), and asserts the positive entries survive while a `None` entry is dropped;
- the same test defines a second loader's copy of an existing name and asserts a full flush (positive control);
- the doc names the `selection_error` rows as kept.

The proxy-define A/B in `zgc-zs9nd8-*` still decides the flip.

## 2. The SATB flush CPU span reads the clock on the no-pool arm and misses the filter (mk; concurrent mark is opt-in)

`hand_thread_batch_to_the_marker` (`gc/src/zgc.rs:8078`) starts a thread-CPU span before it looks for a running pool. When there is none it returns `false`, the caller pushes into the ingress and calls `hand_satb_batch_to_the_marker`, which starts a second span and returns at `None => return` (`zgc.rs:8006`). That costs two thread-clock reads per flush for nothing; on Linux `CLOCK_THREAD_CPUTIME_ID` is a real syscall.

The flush's own filter is outside both spans: the registry screen and the `mark_is_set` pass over the batch (`zgc.rs:7900-7925`). So `satb_flush_ms=` understates the flush half that the every-trigger proposal's T2 term uses.

The default path pays nothing: the SATB barrier is armed only by a concurrent cycle, and `Z_CONC_START_PERCENT_DEFAULT = 0`.

**Closes when:**
- the span opens after the pool is found, and the caller's filter is inside it (one span per flush);
- row MK6 of `t3-mk-report.md` is re-read with the change;
- the kill control `CRATONVM_ZGC_MARK_CPU_ACCOUNTING=0` prints `satb_flush_ms=0`.

## 3. The refill-gate census is a shared counter on every ZGC run, and counts a lost election as a collection (t4/ch)

- **A shared counter.** `note_zgc_gate_refused` (`vm/src/runtime/interpreter/gc_and_alloc.rs:10463`) is a `fetch_add` on one per-VM cache line per refused compiled refill. It is on by default, with no kill switch. In the crawl regime it measures, every compiled thread is on that path. The arena lock that each per-object allocation takes probably dominates, but no row measures it.
- **A lost election counted as a collection.** `zgc_refill_gate_collect` counts `Some(false)` as `gate_collects`. `Some(false)` from `maybe_gc_forced_collected_stamped` means the thread took part in another thread's pause; it did not run one.

**Closes when:**
- row CH-2 is read twice, once with the census line and once with the counter compiled out (or moved behind `CRATONVM_GC_STATS`); CPU must be within 1.02x;
- `Some(false)` gets a census field of its own (`gate_collects_joined=`).

## 4. The booked-uncommit test's positive half cannot fail (rl)

`zt3_rl_the_booked_uncommit_pass_charges_only_its_own_share` (`gc/src/zgc.rs`, `zr4_un_tests`) asserts `last.uncommit_ns >= after.uncommit_ns` on a cumulative counter, over a second pass that released nothing. Its own comment defers the positive reading to a host row, and none is listed.

**Closes when:**
- a Linux-only arm, as in mk's X1-13 fix, asserts a strictly positive `uncommit_ns` after a pass that released at least 16 MiB with accounting on;
- or the rl report lists a host row that shows `uncommit_ms=` above 0 on `HeapGiveBack`.

## 5. The quarantine straddle probe stops 256 MiB below the span (rl, opt-in quarantine)

`release_evac_quarantine` (`gc/src/zgc.rs`, the `straddled` term added by t3/rl) looks for a base no further down than `MAX_ALLOC_EXTENT`, which is `16 Mi x 16 + header` = 256 MiB. If a live array's body is longer than that and a held span lies more than 256 MiB above its base, no base is found and the span is released under the live object.

It is low risk: it needs `CRATONVM_ZGC_EVAC_QUARANTINE=1` and a span already wrongly inside a live object, which is the case the probe exists for.

**Closes when** the probe falls back to the huge-extent table (`prune_huge_extents`' set) for an array above `MAX_ALLOC_EXTENT`, with a unit test that plants a held span 300 MiB into a registered huge extent.

## 6. The closing collection's cause is not checked against HotSpot (mk, concurrent cycles opt-in)

`zgc_full_request_closes_open_cycle` (`gc_and_alloc.rs:15073`) closes the open cycle on `GcDoor::AllocationThreshold`. HotSpot completes the running cycle under the cause that STARTED it (`Warmup`, `Allocation Rate`, `Proactive`, and so on), in `-Xlog:gc`, in `GcInfo` and in JFR. No row compares the closing collection's cause line.

**Closes when:**
- `Zt1MkSystemGcFresh` (or a sibling) prints the cause of the collection before each `System.gc()` collection, from `GarbageCollectionNotificationInfo.getGcCause()`;
- that cause is compared with the HotSpot twin under `CRATONVM_ZGC_CONC_START=20`.

## 7. Generational wholesale roots on young cycles (gn GN-6), restated as a soundness item

`scan_collection_overlays` and `conditional_loader_metadata` answer `true` unconditionally on ZGC (`vm/src/memory/native_roots.rs:388`, `vm/src/memory/roots.rs:209`). They rely on "ZGC's mark is always the precise owner-based marker, which calls `external_roots_for_owner` from every confirmed-live object".

In ZGC's generational mode a young cycle does not trace a pre-marked OLD owner. So an overlay or side-table value deferred to an old owner reaches the young marker only if `young_extra_roots`' section (0) lists it, or a card covers it.

GN-6 calls this unaudited. It is the same class as `zgc-zr1gn-*` and is listed here so the ZGC `=> true` arms are part of that audit.

**Closes when** the gn audit shows that every deferred owner-edge value of an old owner is either a young-cycle root or carded at the store, or adds `young_extra_roots` coverage. Deciding probe: an overlay-backed collection (the `common-b` overlay probes) promoted old, then a young value added through the native table, then minors only, under `CRATONVM_ZGC_GENERATIONAL=1`.
