# rx27 round close: audit of the rx27- / j27- pages and the two parents

Status: audit (round rx27 lane R, 2026-10-29, at `48a464bc73`). Code only, nothing built. No page has been moved
or edited by this audit.

States:

* **FIXED+tested**: the page's own unit test or probe ran on a host binary.
* **FIXED-untested**: code is in, but its test is in a crate suite not reported run, or is `#[ignore]`d, or the
  page's probe is not in the run batteries.
* **MEASURED-no-gain**: built and measured; it stayed under the 15 % bar or was removed.
* **OPEN**: the issue remains, or the change is unmeasured.

Host evidence these states rest on (coordinator, 2026-10-29):

* the jit lib suite passed on int16, with only 5 pre-existing failures (`gci_i41rv`, `r17_multiplies`,
  `r17w2_irfeed`, the `r23w8c` kill switch, safepoint poll arg1);
* the types and vm suites are green;
* the R24 facts and the GC battery equal HotSpot under every arm.

Suites not reported run: `cratonvm-native-builtins`, `cratonvm-native-collections`, `cratonvm-native-api`. Tests that
need `--include-ignored` are not counted either.

Abbreviations in the evidence column:

* "dse0": the chain switch set the coordinator adopted on 2026-10-29 (`CHAIN_DEEP_SPINE` ON, `CHAIN_ENTRY_SPLICE`
  opt-in).
* `allc`: the chain bundle (entry + in-loop + predicate pairs + the cold handler arm).
* `pre` / `prez` / `oip` / `rall2` / `azt` / `azg`: the arms defined on the regalloc and find-email pages
  (`prez` = `pre` + `azt` + `azg`; `rall2` = `azt` + `azg` + `ebb`; `oip` = operand-in-place).

## Parents

| path | issue | state | evidence |
|---|---|---|---|
| `jit/r23-w2-regex-rows-gap-is-per-call-cost-20261021.md` | Matcher rows over 3x HotSpot | OPEN | Lane prof b17: regex-matches 14.7k vs HotSpot 3.8k (3.9x), find-email 36.2k vs 9.7k (3.7x), find-ci 24.5k vs 7.3k (3.3x), matchonly 27.0k vs 7.0k (3.9x), regex-groups 21.3k vs 5.8k (3.7x). |
| `jit/r23-w5-compiled-jdk-string-split-and-regex-bodies-7-13x-20261021.md` | split rows over 3x HotSpot | OPEN | split-comma 1.8x and split-comma-10 1.7x (substring prefix default ON, 0.77x), list 2.1x: inside the gate. Still over: split-ws 3.6-3.9x (b17 49.7k, range 40.4-53.0k), cached 3.04x (35.8k, dse0 on bimodal-10), compile 5.5x (4.38k). |

## rx27 pages (`docs/known-issues/jit/`)

| path | issue | state | evidence |
|---|---|---|---|
| `rx27-R-anewarray-loop-callees-flip-makes-w13-6-inert-20261028.md` | the j27 flip of `_SPLICE_ANEWARRAY_LOOP_CALLEES` disables W13-6 | OPEN | No host A/B recorded. The last reading is still round 25's: `cached` +12.6 %, `matchonly` +8.3 %. |
| `rx27-R-effective-defaults-differ-from-inventory-20261028.md` | six keys listed opt-in but ON through a parent | OPEN | The inventory generator is unchanged. The bundled off-word fix is FIXED+tested: `6d80b86b25`, `rx27_the_bundle_yields_to_every_off_word` (jit lib). |
| `rx27-R-wave3-review-residuals-20261028.md` | top-level subSequence carve; LATIN1 guard withdrawal; redefined handler-bearing scope | OPEN (items 2-3) | Item 1 FIXED+tested: `top_level_string_prefix_site_stays_call`, `a_non_string_profiled_seq_subsequence_site_keeps_its_splice` (jit lib). Outer-hoist kill FIXED+tested: `b989d085fe`, `rx27_the_outer_hoist_yields_to_an_explicit_kill`. Items 2-3 host checks not recorded. |
| `rx27-R-wave4-review-residuals-20261028.md` | `saveProperties` in `--jdk-only`; `elementData` component class; poll / AND host checks | FIXED+tested | Item 1: "Checked on a binary 2026-10-28 (orchestrator): parity gain, closed". Item 2: `rx27_w4_object_array_screen_tests::rx27_only_an_exact_object_array_passes_the_add_screen` (jit lib). Item 3: `POLL_TRUE_BACK_EDGES` removed (`5ea831d133`); `AND_ZERO_TEST_BRANCH` measured opt-in (regalloc page). |
| `rx27-alloc-zgc-compiled-allocation-is-30-instructions-over-hotspot-20261028.md` | ZGC compiled allocation about 30 instructions over HotSpot | OPEN | "By reading, not measured". `ZGC_LEAN_START_BIT` / `_BTS` are opt-in with no A/B. `zr9bo_short_announce_tests` (jit lib) ran. |
| `rx27-bimodal-matchonly-and-groups-spread-20261028.md` | matchonly / groups land in 2-3 levels per process | OPEN | Still bimodal on b9 and b17 (def 25.7-28.4k). `NESTED_HOT_CAP_BY_FREQUENCY` stays opt-in ("not a measured win"). |
| `rx27-bimodal-per-activation-cost-of-large-ir-bodies-20261028.md` | about 110 + 40 instructions per activation of a large body | OPEN (levers landed) | `FRAME_BLOCK_DESCRIPTOR` default ON: regex-matches -1.5 %, compile -6.7 % (b11). GC battery identical on every collector; `rx27_frame_block_descriptor_*` tests. Keep-loop rules landed. Three clear switches removed ("never paid"). |
| `rx27-chain-compile-row-helpers-and-call-write-summaries-20261028.md` | compile row: parse bodies stay calls; field rereads across calls | OPEN | `SPLICE_COLD_ARM_EDGE_SET` / `CALL_WRITE_SUMMARY` are opt-in; their tests ran (jit lib); no A/B recorded. compile is 4.38k vs 800 (5.5x). |
| `rx27-chain-grouptail-pair-is-gated-by-the-global-nested-bimorphic-switch-20261028.md` | `GroupTail` pc 70 pair gated by the global switch | MEASURED-no-gain | Global switch: regex-groups 0.92x, regex-matches 0.96x (rx27b2, below the bar). `CHAIN_BIMORPHIC_LINKS` opt-in; `rx27_chain_a_chain_sites_pair_is_kept_without_the_global_switch` (jit lib). |
| `rx27-chain-loop-root-rule-can-replace-the-anewarray-loop-refusal-20261028.md` | proposal: loop-root rule replaces the anewarray refusal | OPEN | Proposal. `LOOP_ROOT_OWN_COMPILE` "measured flat" (split-self page); the A/B named on the page is not recorded. |
| `rx27-chain-matcher-entry-group-and-predicate-pairs-20261028.md` | chain entry, `Matcher.group`, predicate pairs stay calls | MEASURED-no-gain (bundle flipped default ON 2026-10-29, pending build) | b9: `allc` regex-matches 0.88x (disjoint), regex-groups 0.96x (overlapping), the rest 1.00. `ent` alone: regex-groups +11 %. Facts equal HotSpot. |
| `rx27-chain-pattern-split-self-per-element-work-20261028.md` | `cached`: `Pattern.split` 29 % self | OPEN | By reading only, not profiled in the lane. |
| `rx27-chain-regex-groups-spine-is-deeper-than-the-builder-frame-cap-20261028.md` | second group past the chain-tree and frame caps | FIXED+tested | `CHAIN_DEEP_SPINE` default ON (dse0): cached -21 %, split-ws -13 %, regex-split -5 %, facts and deopt rows identical (bimodal-10). Tests `rx27_deep_spine_*` (jit lib). regex-groups itself is still over the gate (parent). |
| `rx27-compat-battery-rows-that-differ-in-both-modes-20261028.md` | battery rows that differ from HotSpot in both modes | OPEN | Triage only. |
| `rx27-compile-pattern-compile-residuals-at-tip-20261028.md` | `compile` row outside type checks | OPEN | compile 4.36k vs 803 (rx27b2); the proxy-ways arm read 0.95x (inside the bar). |
| `rx27-compile-single-pass-typechecks-have-no-proxy-ways-20261028.md` | single-pass instanceof sends every lambda proxy to the helper | OPEN | `SP_INSTANCEOF_PROXY_WAYS` opt-in; `rx27_single_pass_instanceof_proxy_ways_answer_seen_proxies_inline` (jit lib) ran. No host census or A/B, which the page's retire condition needs. |
| `rx27-find-email-reg-moves-are-residency-copies-20261029.md` | find-email reg moves are residency copies | MEASURED-no-gain | b17: `pre` -0.5 % (overlapping); `prez` -2.2 % (disjoint, the and-zero folds); `oip` find-ci -2.6 % (disjoint). All below the bar. Facts 16/16. |
| `rx27-guards-compact-field-test-residual-20261028.md` | per-character compact-layout field test | MEASURED-no-gain | Close-out: `TRUST_BOOT_PRIM_ARRAY_FINALS` / `BCE_CONST_LENGTH_RANGE` / `POLL_TRUE_BACK_EDGES` removed (`5ea831d133`); `PROVEN_FIELDS_HOIST` removed (`ddbbf53f16`), "matchonly -0.7 %, no row disjoint". |
| `rx27-matcher-fence-stripped-body-liveness-misses-the-fence-handler-20261028.md` | fence-stripped body liveness missed the handler | FIXED+tested | `rx27_a_fence_stripped_body_keeps_its_fenced_local_live_on_the_throw_path` (jit lib, int16). |
| `rx27-matcher-loop-root-moves-into-a-caller-by-receiver-resolution-20261028.md` | loop root moves into a caller by receiver resolution | OPEN | Proposal; "Nothing measured here". |
| `rx27-matcher-redefined-chain-resumes-a-masked-splice-copy-20261028.md` | redefined chain scope resumes the masked copy | FIXED+tested | `rx27_splice_scope_sources_resume_the_original_not_the_masked_copy` (jit lib). A redefinition-battery run is not recorded. |
| `rx27-regalloc-frame-slot-traffic-under-the-accumulator-lowering-20261028.md` | frame-slot traffic of the accumulator lowering | MEASURED-no-gain | Close-out: `azg` find-email -1.0 %, `rall2` -2.0 %, `oip` -2.6 % / -3.0 % (disjoint, below bar); `HOME_STORE_ELIDE` and `PHI_SOURCE_COALESCE` removed; the rest noise. |
| `rx27-scanloop-outer-position-loop-invariants-20261028.md` | position loop re-reads node fields per character | OPEN | P-1 / P-2 still open. `SEQ_CHARAT_OUTER_HOIST` opt-in, unmeasured (W13-4: hoist lost 3-8 %). `rx27w3_*` / `rx27w4_*` tests ran. |
| `rx27-splitfast-arraylist-add-and-sublist-toarray-per-field-20261028.md` | `ArrayList.add` / `SubList.toArray` per field | OPEN | `ARRAYLIST_ADD_PREFIX` opt-in; the A/B (`add:` / `both:` arms on r23-w5) is not recorded. |
| `rx27-splitfast-copyofrange-byte-prefix-proposal-20261028.md` | IR `Arrays.copyOfRange([BII)` prefix | OPEN | Implemented opt-in; `rx27_copy_of_range_copies_exactly_the_range_or_declines` ran. The `cor:` A/B is not recorded. |
| `rx27-w4-per-row-instruction-breakdown-20261028.md` | per-row instruction breakdown (measurement) | OPEN | A measurement page; it names levers, it fixes nothing. |

## j27 pages (`docs/known-issues/jit/` unless noted)

| path | issue | state | evidence |
|---|---|---|---|
| `j27-argfix-spliced-owned-args-two-level-replay-proposal-20261028.md` | owned-arg two-level replay | FIXED+tested (opt-in) | `j27_argfix2_*` (jit lib); `CALLEE_OWNED_ARGS_SPLICE_REPLAY=1`. |
| `j27-argfix2-owned-arg-window-refuses-every-spliced-call-20261028.md` | owned-arg window refuses every spliced call | FIXED+tested (opt-in) | `j27_argfix2_*` (jit lib). |
| `j27-c2-accept-memo-row-opt-in-but-reader-default-on-20261028.md` | memo row opt-in, reader default ON | FIXED+tested | `j27_latches_tests` (jit), `flag_default_matches_reader` (types). |
| `j27-clone-spliced-instance-sites-and-inline-expansion-proposal-20261028.md` | spliced instance `clone()`, inline clone | FIXED+tested (partial) | `j27_clone_virtual_thin_tests`, `j27_clone_copy_tests` (jit / vm). Item 2 for instances is not done. |
| `j27-compat-ofarray-numeric-heap-segment-downcall-is-permitted-20261028.md` | heap `ofArray` segment admitted by a plain downcall | FIXED-untested | Tests are in native-builtins (`plain_downcall_refuses_every_ofarray_segment`); `J27FfmRows` is not in the reported batteries. |
| `j27-ffm-heap-segment-tostring-and-bounds-texts-diverge-20261028.md` | `MemorySegment` texts diverge from HotSpot | FIXED-untested (partial) | `panama::tests::*` (native-builtins). Native vs mapped kept by design. |
| `j27-hot-switch-latches-to-memo-slots-proposal-20261028.md` | hot switches as `MemoSlot`s | OPEN | Proposal, not implemented. |
| `j27-intrinsic-coverage-vs-hotspot-20261028.md` | intrinsic coverage vs C2 | OPEN | Audit work list. Three rows fixed, "HOST-VERIFIED s4" (`pure_expand`). |
| `j27-java-loader-defines-skip-the-new-subtype-listener-20261028.md` | Java-loader defines skip the subtype listener | FIXED+tested | `j27_subtype_define_tests` (vm, green). |
| `j27-jni-get-array-elements-oom-pends-oome-20261028.md` | `Get<T>ArrayElements` pends OOME | FIXED+tested | `native::jni::tests::j27_get_array_elements_failed_copy_is_null_without_oome` (vm). |
| `j27-jni-getstringchars-and-critical-copy-oom-pend-oome-20261028.md` | `GetStringChars` / critical copy pend OOME | FIXED+tested | `native::jni::tests::j27_string_chars_and_critical_failed_copy_is_null_without_oome` (vm). |
| `j27-latin1-character-class-expand-proposal-20261028.md` | expand Latin-1 `Character.is*` | OPEN | Proposal. |
| `j27-loop-phi-residency-vs-flipped-population-switches-20261028.md` | loop-phi residency vs flipped switches | FIXED-untested | Its tests need `loop_phi_residency_tests -- --include-ignored`, which is not counted. |
| `j27-loopcg-counted-loop-poll-and-residency-proposal-20261028.md` | strip-mined poll; resident unrolled temps | FIXED+tested (partial, opt-in) | `j27_remat*`, `j27_residency*`, `j27_stripmine*` (jit lib). The remaining halves are open. |
| `j27-mh-astype-not-a-number-unbox-frames-20261028.md` | asType unbox of a non-Number lists no frames | FIXED-untested | `lang_invoke::call_site::j27_wmte1_unbox_raise_chain_tests` is in native-builtins (not reported run). |
| `j27-mh-real-astype-converting-bind-proposal-20261028.md` | converting / bound-static REAL handles | OPEN | Proposal, not built. |
| `j27-monrows-monitor-sync-door-rows-20261028.md` | monitor / sync-door backlog rows | FIXED+tested (partial) | `monitor_rows`, `j27_sdb17_3`, `r17_monitor5`, ... (jit lib). SDB17-3 needs a measurement; two rows are not live. |
| `j27-osr-splice-bind-refuses-cold-indy-trap-callee-proposal-20261028.md` | OSR binder refuses a cold-indy-trap callee | OPEN (premise void) | Lane INDYTRAP: "premise does not hold for javac's concat". Its splice half is the concat page below. |
| `j27-post-clinit-fixup-fallbacks-publish-wrong-class-20261028.md` | fixup fallbacks publish the wrong class | OPEN | "open, not fixed". |
| `j27-post-clinit-fixup-hard-coded-instance-widths-20261028.md` | fixup arms use hard-coded widths | FIXED+tested | `post_clinit_fixup_instance_width_tests` (vm). |
| `j27-r18-backlog-live-rows-20261028.md` | live round-18 backlog rows | OPEN (partial) | CD16-1 and CB17-6 fixed; the others are HOST / CODE rows. |
| `j27-reflect-new-array-prefix-posts-no-vmobjectalloc-20261028.md` | `Array.newArray` prefix skips `VMObjectAlloc` | FIXED+tested | `r23w8_reflect_new_array_resolve_tests` (jit lib). |
| `j27-seglongloops-heap-sum-bistable-slow-state-FIXED-20261028.md` | `heap-sum` bistable slow state | FIXED+tested | Candidate 2 fixed in code, host-verified s8; moved to `internal/fixed-bugs/jit/`. |
| `j27-small-call-frame-arms-and-osr-super-bind-proposal-20261028.md` | frame arms; OSR super bind | FIXED+tested (P2 only, opt-in) | `osr_guarded_super_bind` (jit lib). P1 and P3 untouched. |
| `j27-splice-ffm-element-hangs-seglongloops-20261028.md` | `SPLICE_FFM_ELEMENT=1` hangs `SegLongLoops` | FIXED-untested (partial) | No unit test named; `R26SegLongLoops` / `R23SegMono` are not in the reported batteries. |
| `j27-splice-refuses-callee-with-arg-taking-concat-proposal-20261028.md` | arg-taking concat callee never spliced | FIXED+tested (opt-in) | `x64::tests::j27_splicecat_*` (jit), `splice_concat_rows::tests::*` (vm). |
| `j27-spliced-typecheck-helper-call-drops-the-callee-frame-20261028.md` | spliced helper call drops the callee frame | FIXED+tested | `x64::tests::j27_a_spliced_typecheck_helper_call_keeps_the_callee_frame`, `..._static_field_helper_call_...` (jit lib). |
| `j27-static-dispatch-counter-compiles-on-its-own-clock-20261028.md` | static dispatch counter compiles on its own clock | FIXED+tested | `i93_callee_door_defers_to_worker_tests` (vm). |
| `j27-static-varhandle-slot-memo-proposal-20261028.md` | static `VarHandle` slot memo | FIXED-untested | Implemented (FFMVH) and measured on Windows only; no named test. |
| `j27-stripmine-osr-and-array-bodies-proposal-20261028.md` | strip-mine OSR and array bodies | FIXED+tested (partial, opt-in) | `strip_mine` (jit lib). rx27 wave 2: the battery failure was the fast-boot prune, now fixed. |
| `j27-unmodifiable-views-snapshot-iterators-20261028.md` | unmodifiable-view snapshot iterators | FIXED-untested (OBSOLETE in default arms) | `j27_unmodifiable_view_factory_registration_tests` is in native-collections (not reported run). |
| `j27-vh-gen-card-edge-inline-check-proposal-20261028.md` | VH gen-card edge inline check | FIXED+tested | `j27_vhcard_inline_edge_tests`, `j27_vhcard_sp_edge_tests` (jit lib). Perf measured on Windows only. |
| `docs/known-issues/j27-compat-fast-boot-residual-service-loader-and-natives-20261028.md` | `--compatible` fast-boot ServiceLoader / CLDR / savedProps / seed | OPEN (Windows) | "Linux rows FIXED, VERIFIED AGAINST A BINARY 2026-10-28"; the Windows rows are open. |
