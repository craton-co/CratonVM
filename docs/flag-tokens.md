# CratonVM — complete `CRATONVM_*` token reference

*Generated from `types/src/flag_groups.rs::INVENTORY`. Regenerate with
`tools/flag-census/render-tokens.sh`; `types/tests/flag_docs_generated.rs`
fails the build if this table and the code disagree about which tokens exist.
(That claim used to name `flag_surface.rs`, which only ever compared
`INVENTORY` against `flag-surface.txt` and never read this file — which is how
this table came to be three rows wrong.)*

Every knob in the VM is a token in one of **ten** environment variables. This
file lists all of them. [`docs/CONFIG.md`](CONFIG.md) documents the handful you
would set when *running* an application; the rest are here because a complete
list is what stops the surface from growing back.

## Syntax

```sh
CRATONVM_<GROUP>=token,token=value,-token
```

| Form | Meaning |
| --- | --- |
| `token` or `+token` | switch it **on** |
| `-token` | switch it **off** |
| `token=value` | switch it on with a value (sizes, counts, paths) |
| `all` | every token in that group |

Tokens are comma-separated and whitespace around them is ignored. A token the
group does not define prints one line on stderr and is otherwise ignored — a
typo is loud, which it was not before.

**Setting a legacy JIT switch directly.** A grouped token writes `1` into its
legacy variable. If you export that variable yourself, the JIT's boolean switches
(`CRATONVM_JIT_*`, `CRATONVM_NO_*`, `CRATONVM_DBG_*` and the other names the JIT
crate reads as on/off) treat `0`, `false`, `off`, `no` and the empty string as
**off**. Matching is case-insensitive and ignores surrounding whitespace. Any
other value is on. (Previously these switches only checked whether the
variable was set, so `CRATONVM_JIT_RANGE_BCE=0` turned the pass *on*.) A few
JIT-read names that carry a value (a number, a word such as `nostore`), or that
the collector also reads as a gate, still check only for presence. See
`jit-presence-only-flag-reads-FIXED.md` for the list.

## The five scalars

These keep their own name: they are a path, a location or a master switch, not
a knob with an on/off sense.

| Variable | Meaning |
| --- | --- |
| `CRATONVM_JAVA_HOME` | JDK to boot from; overrides `JAVA_HOME` for the boot probe. |
| `CRATONVM_BIN` | Path to the `cratonvm` binary, for harnesses that re-exec it. |
| `CRATONVM_MAVEN_REPO_LOCAL` | Local Maven repository root. |
| `CRATONVM_ENABLE_ASSERTIONS` | Evaluate Java `assert` statements. Set for you by an unscoped `-ea` on the command line; setting it directly is only needed for a launcher that cannot pass VM flags. |
| `CRATONVM_DISABLE_JIT` | Interpreter-only execution. Also set by `--nojit`. |

## Legacy names

The **Expands to** column is not documentation of a second surface — it is the
compatibility rule. Setting `CRATONVM_DBG_GC_STRESS=65536` directly still works
because that variable is exactly what `CRATONVM_DBG=gc-stress=65536` writes. The
launcher prints one line naming the grouped spelling when it sees one.

Where a row lists two variables (`A / B`), those were two spellings of one
switch before this table existed — `A` was the opt-in and `B` the opt-out, or
`A` was the real implementation and `B` the synthetic shim. They are now one
token: the plain form selects `A`, the `-` form selects `B`.

A grouped variable **wins** over a legacy one, so `-token` can mask a stale
export inherited from a parent shell.

## Which collector a token binds

A token's name says what it does; it does not say which backend reads it. Most
of the surface is unambiguous because the name carries the collector
(`zgc-*`, `g1-*`). Two GC tokens are not, and both reach a collector the reader
would not guess. This section is hand-written and lists only the cases where the
name is actively misleading — it is not a second inventory.

| Token (legacy name) | Generational | G1 | ZGC |
| --- | --- | --- | --- |
| `-moving-young` (`CRATONVM_NO_MOVING_YOUNG=1`) | **binds** — turns off young evacuation | **binds** — the same process-wide switch gates G1's moving decision | **binds, with a caveat below** |
| `zgc-relocate-proven-jit=0` (`CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0`) | no | no | **binds, with the same caveat** |
| `zgc-relocate=0` (`CRATONVM_ZGC_RELOCATE=0`) | no | no | binds unconditionally — the slide never runs |

**The caveat, and it is the surprising part.** ZGC's relocation refusal is a
first-failing-term chain, and the whole chain is applied only on a cycle where
a **compiled frame is live**
(`gc_quiescence::is_active() || unregistered_jit_frame_on_stack()`). Four of its
six terms — including both rows above — are computed on every cycle and
discarded when no compiled frame is live, because every term in the chain is an
obligation *about compiled frames* and with none live the obligations are
discharged vacuously.

So on a JIT-cold cycle, `CRATONVM_NO_MOVING_YOUNG=1` and
`CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0` **do not stop ZGC compacting**. Set
either on a warm workload and ZGC refuses on every JIT-warm cycle and compacts
on every cold one.

`CRATONVM_NO_MOVING_YOUNG` binding ZGC at all is deliberate, not an accident of
a shared predicate: it is the process-wide "do not move objects out from under
compiled frames" switch, and the whole rewritable-root contract ZGC's chain
leans on (`moving_young_coverage_incomplete`, the `MOVABLE_BOUNDS` publication,
the conservative-scan suppression) stops being computed when it is off. A ZGC
that kept sliding then would be relocating on a proof nothing is producing.
The reasoning is written down at
`zgc::relocation_skip_reason::MOVING_YOUNG_DISABLED`.

**Reading it on a run.** `--verbose:gc` prints two censuses over the same index
space:

* `[GC] zgc-relocation-refusal-term: <term>=N` — the term was violated on N
  cycles, counted where the verdict is computed;
* `[GC] zgc-relocation-skip-reason: <term>=N` — the term actually stopped a
  slide on N cycles.

The gap between them is the bypass. A term that appears on the first line with
no matching second line fired on every cycle it was asked about and stopped none
of them. If you need the refusal unconditionally, use `zgc-relocate=0`, which
has no such gate.

## `CRATONVM_DBG`

36 tokens.

| Token | Expands to |
| --- | --- |
| `jit-deopt-a-lot` | `CRATONVM_DBG_JIT_DEOPT_A_LOT` |
| `gc-at-back-edge` | `CRATONVM_DBG_GC_AT_BACK_EDGE` |
| `mh-link-census` | `CRATONVM_DBG_MH_LINK_CENSUS` |
| `bytecode-only-body` | `CRATONVM_DBG_BYTECODE_ONLY_BODY` |
| `deopt-eager` | `CRATONVM_DEOPT_EAGER` |
| `deopt-verify` | `CRATONVM_DEOPT_VERIFY` |
| `diag-properties` | `CRATONVM_DIAG_PROPERTIES` |
| `diag-serviceloader` | `CRATONVM_DIAG_SERVICELOADER` |
| `eintr-inject` | `CRATONVM_DBG_EINTR_INJECT` |
| `eintr-no-retry` | `CRATONVM_DBG_EINTR_NO_RETRY` |
| `force-moving` | `CRATONVM_DBG_FORCE_MOVING` |
| `getresources` | `CRATONVM_DBG_GETRESOURCES` |
| `iae-trace` | `CRATONVM_IAE_TRACE` |
| `invokestatic-loader-trace` | `CRATONVM_INVOKESTATIC_LOADER_TRACE` |
| `native-shadow-sink-cap` | `CRATONVM_NATIVE_SHADOW_SINK_CAP` |
| `exit-report-file` | `CRATONVM_EXIT_REPORT_FILE` |
| `nativelibraries-load-ok` | `CRATONVM_DBG_NATIVELIBRARIES_LOAD_OK` |
| `nsee-trace` | `CRATONVM_NSEE_TRACE` |
| `phase-accounting` | `CRATONVM_PHASE_ACCOUNTING` |
| `phase-accounting-jfr` | `CRATONVM_PHASE_ACCOUNTING_JFR` |
| `phase-accounting-out` | `CRATONVM_PHASE_ACCOUNTING_OUT` |
| `zgc-parmark-fail` | `CRATONVM_DBG_ZGC_PARMARK_FAIL` |
| `selector` | `CRATONVM_DBG_SELECTOR` |
| `soft-exit` | `CRATONVM_SOFT_EXIT` |
| `stale-objref-cycles` | `CRATONVM_DBG_STALE_OBJREF_CYCLES` |
| `surefire-ipc-dbg` | `CRATONVM_SUREFIRE_IPC_DBG` |
| `sweep-zero` | `CRATONVM_DBG_SWEEP_ZERO` |
| `trace-sb-filter` | `CRATONVM_TRACE_SB_FILTER` |
| `watch-cell` | `CRATONVM_DBG_WATCH_CELL` |
| `jni-repeat-audit` | `CRATONVM_DBG_JNI_REPEAT_AUDIT` |
| `boot-image-trace-addrs` | `CRATONVM_BOOT_IMAGE_TRACE_ADDRS` |
| `root-kind-census` | `CRATONVM_DBG_ROOT_KIND_CENSUS` |
| `access` | `CRATONVM_DBG_ACCESS` |
| `ir-linear-scan` | `CRATONVM_DBG_IR_LINEAR_SCAN` |
| `modload` | `CRATONVM_DBG_MODLOAD` |
| `gpu-trace-bytes` | `CRATONVM_GPU_TRACE_BYTES` |

## `CRATONVM_JIT`

147 tokens.

| Token | Expands to |
| --- | --- |
| `sp-ic-deny` | `CRATONVM_JIT_SP_IC_DENY` |
| `sp-ic-only` | `CRATONVM_JIT_SP_IC_ONLY` |
| `ctor-direct-call` | `CRATONVM_NO_CTOR_DIRECT_CALL` |
| `osr-ctor-bind` | `CRATONVM_NO_OSR_CTOR_BIND` |
| `deopt-real` | `CRATONVM_DEOPT_REAL` |
| `force-c2` | `CRATONVM_JIT_FORCE_C2` |
| `inline-tlab-new` | `CRATONVM_NO_JIT_INLINE_TLAB_NEW` |
| `inline-tlab-newarray` | `CRATONVM_NO_JIT_INLINE_TLAB_NEWARRAY` |
| `intrinsics` | `CRATONVM_DISABLE_INTRINSICS` |
| `ir-branchy` | `CRATONVM_NO_IR_BRANCHY` |
| `ir-call` | `CRATONVM_JIT_IR_CALL` |
| `profile-save` | `CRATONVM_JIT_PROFILE_SAVE` |
| `profile-load` | `CRATONVM_JIT_PROFILE_LOAD` |
| `print-compilation` | `CRATONVM_JIT_PRINT_COMPILATION` |
| `ir-splice-multi-return` | `CRATONVM_JIT_IR_SPLICE_MULTI_RETURN` |
| `ir-elide-unnamed-homes` | `CRATONVM_JIT_IR_ELIDE_UNNAMED_HOMES` |
| `ir-elide-homes-spill-at-inspection` | `CRATONVM_JIT_IR_ELIDE_HOMES_SPILL_AT_INSPECTION` |
| `ir-site-named-loop-values` | `CRATONVM_JIT_IR_SITE_NAMED_LOOP_VALUES` |
| `operand-fold` | `CRATONVM_JIT_NO_OPERAND_FOLD` |
| `mh-bound-type-table-app-classes` | `CRATONVM_MH_BOUND_TYPE_TABLE_APP_CLASSES` |
| `mh-intern-descriptor-types` | `CRATONVM_MH_INTERN_DESCRIPTOR_TYPES` |
| `mh-loader-scoped-types` | `CRATONVM_MH_LOADER_SCOPED_TYPES` |
| `inline-card-mark` | `CRATONVM_JIT_INLINE_CARD_MARK` |
| `ir-loop-lock-coarsen-blocks` | `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_BLOCKS` |
| `ir-loop-lock-coarsen-instance-blocks` | `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_INSTANCE_BLOCKS` |
| `ir-sink-equal-depth` | `CRATONVM_JIT_IR_SINK_EQUAL_DEPTH` |
| `ir-param-copy` | `CRATONVM_JIT_IR_PARAM_COPY` |
| `metrics` | `CRATONVM_JIT_METRICS` |
| `metrics-out` | `CRATONVM_JIT_METRICS_OUT` |
| `metrics-ring` | `CRATONVM_JIT_METRICS_RING` |
| `ir-nested-chain-splice` | `CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE` |
| `ir-nested-lambda-iface` | `CRATONVM_JIT_IR_NESTED_LAMBDA_IFACE` |
| `ir-chain-inner-loop-proofs` | `CRATONVM_JIT_IR_CHAIN_INNER_LOOP_PROOFS` |
| `ir-chain-deep-spine` | `CRATONVM_JIT_IR_CHAIN_DEEP_SPINE` |
| `ir-chain-entry-splice` | `CRATONVM_JIT_IR_CHAIN_ENTRY_SPLICE` |
| `ir-nested-in-loop-hot` | `CRATONVM_JIT_IR_NESTED_IN_LOOP_HOT` |
| `ir-loop-root-own-compile` | `CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE` |
| `ir-loop-root-own-compile-strips` | `CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE_STRIPS` |
| `native-matcher-find` | `CRATONVM_NATIVE_MATCHER_FIND` |
| `regex-matcher-bytecode` | `CRATONVM_REGEX_MATCHER_BYTECODE` |
| `native-pbe-keyfactory` | `CRATONVM_NATIVE_PBE_KEYFACTORY` |
| `native-string-regex` | `CRATONVM_NATIVE_STRING_REGEX` |
| `osr` | `CRATONVM_JIT_OSR` |
| `local-handlers` | `CRATONVM_JIT_LOCAL_HANDLERS` |
| `implicit-local-handlers` | `CRATONVM_JIT_IMPLICIT_LOCAL_HANDLERS` |
| `avx512` | `CRATONVM_AVX512` |
| `mh-real-ctor-cast-adapter-lane-under-jit` | `CRATONVM_MH_REAL_CTOR_CAST_ADAPTER_LANE_UNDER_JIT` |
| `backedge-poll-gate` | `CRATONVM_JIT_NO_BACKEDGE_POLL_GATE` |
| `field-fast-path` | `CRATONVM_JIT_NO_FIELD_FAST_PATH` |
| `field-addr-elide` | `CRATONVM_JIT_NO_FIELD_ADDR_ELIDE` |
| `arraylength-fast` | `CRATONVM_JIT_NO_ARRAYLENGTH_FAST` |
| `ref-array-fast` | `CRATONVM_JIT_NO_REF_ARRAY_FAST` |
| `system-class-latch` | `CRATONVM_JIT_NO_SYSTEM_CLASS_LATCH` |
| `osr-inline-gate` | `CRATONVM_JIT_NO_OSR_INLINE_GATE` |
| `invoke-fast-door` | `CRATONVM_JIT_INVOKE_FAST_DOOR / CRATONVM_JIT_NO_INVOKE_FAST_DOOR` |
| `nonvirtual-fast-door` | `CRATONVM_JIT_NO_NONVIRTUAL_FAST_DOOR` |
| `cached-native-facts` | `CRATONVM_JIT_NO_CACHED_NATIVE_FACTS` |
| `locals-slab` | `CRATONVM_JIT_NO_LOCALS_SLAB` |
| `shadow-stack` | `CRATONVM_SHADOW_STACK` |
| `sp-coalesce` | `CRATONVM_SP_NO_COALESCE` |
| `threshold` | `CRATONVM_JIT_THRESHOLD` |
| `ir-frame-block-max-homes` | `CRATONVM_JIT_IR_FRAME_BLOCK_MAX_HOMES` |
| `stackwalk-pipeline-layout-once` | `CRATONVM_STACKWALK_PIPELINE_LAYOUT_ONCE` |
| `stackwalk-deferred-lines` | `CRATONVM_STACKWALK_DEFERRED_LINES` |
| `stackwalk-pipeline-invoke-by-id` | `CRATONVM_STACKWALK_PIPELINE_INVOKE_BY_ID` |
| `stackwalk-caller-class-tail` | `CRATONVM_STACKWALK_CALLER_CLASS_TAIL` |
| `stackwalk-window-capture` | `CRATONVM_STACKWALK_WINDOW_CAPTURE` |
| `tier-c1-threads` | `CRATONVM_TIER_C1_THREADS` |
| `tier-c1-threshold` | `CRATONVM_TIER_C1_THRESHOLD` |
| `tier-c2-min-invocations` | `CRATONVM_TIER_C2_MIN_INVOCATIONS` |
| `tier-c2-threads` | `CRATONVM_TIER_C2_THREADS` |
| `tier-c2-standby` | `CRATONVM_TIER_C2_STANDBY` |
| `tier-c2-standby-depth` | `CRATONVM_TIER_C2_STANDBY_DEPTH` |
| `tier-c2-standby-scale` | `CRATONVM_TIER_C2_STANDBY_SCALE` |
| `tier-c2-threshold` | `CRATONVM_TIER_C2_THRESHOLD` |
| `tier-osr-backedge` | `CRATONVM_TIER_OSR_BACKEDGE` |
| `tier-osr-threshold` | `CRATONVM_TIER_OSR_THRESHOLD` |
| `tier-pgo` | `CRATONVM_TIER_PGO` |
| `tiered` | `CRATONVM_TIER_ENABLED` |
| `unroll` | `CRATONVM_JIT_UNROLL / CRATONVM_DISABLE_UNROLL` |
| `vectorize` | `CRATONVM_JIT_VECTORIZE` |
| `verify-arena-order` | `CRATONVM_JIT_VERIFY_ARENA_ORDER` |
| `verify-frame-states` | `CRATONVM_JIT_VERIFY_FRAME_STATES` |
| `verify-ir` | `CRATONVM_JIT_VERIFY_IR` |
| `verify-branches` | `CRATONVM_JIT_VERIFY_BRANCHES` |
| `verify-memory-chain` | `CRATONVM_JIT_VERIFY_MEMORY_CHAIN` |
| `verify-schedule` | `CRATONVM_JIT_VERIFY_SCHEDULE` |
| `verify-types` | `CRATONVM_JIT_VERIFY_TYPES` |
| `arm64` | `CRATONVM_JIT_ARM64` |
| `arm64-safepoints` | `CRATONVM_JIT_ARM64_SAFEPOINTS` |
| `gdb` | `CRATONVM_JIT_GDB` |
| `arith-licm-trim-entered` | `CRATONVM_JIT_ARITH_LICM_TRIM_ENTERED` |
| `vec-ewise-forms` | `CRATONVM_JIT_VEC_EWISE_FORMS` |
| `field-array-hoist` | `CRATONVM_JIT_FIELD_ARRAY_HOIST` |
| `ref-hoist-oop-maps` | `CRATONVM_JIT_REF_HOIST_OOP_MAPS` |
| `aaload-licm-typecheck-fence` | `CRATONVM_JIT_AALOAD_LICM_TYPECHECK_FENCE` |
| `jni-in-native-no-signal` | `CRATONVM_JNI_IN_NATIVE_NO_SIGNAL` |
| `jni-in-native-doors` | `CRATONVM_JNI_IN_NATIVE_DOORS` |
| `jni-in-native-lazy` | `CRATONVM_JNI_IN_NATIVE_LAZY` |
| `ffm-in-native-repeat` | `CRATONVM_FFM_IN_NATIVE_REPEAT` |
| `ffm-downcall-in-native-lazy` | `CRATONVM_FFM_DOWNCALL_IN_NATIVE_LAZY` |
| `ir-phi-coalesce-back-edge` | `CRATONVM_JIT_IR_PHI_COALESCE_BACK_EDGE` |
| `field-array-bce` | `CRATONVM_JIT_FIELD_ARRAY_BCE` |
| `field-int-hoist` | `CRATONVM_JIT_FIELD_INT_HOIST` |
| `sr-slots-past-licm` | `CRATONVM_JIT_SR_SLOTS_PAST_LICM` |
| `ir-splice-frame-states` | `CRATONVM_JIT_IR_SPLICE_FRAME_STATES` |
| `ir-splice-chain-fences` | `CRATONVM_JIT_IR_SPLICE_CHAIN_FENCES` |
| `class-facts-memo-wrapper` | `CRATONVM_CLASS_FACTS_MEMO_WRAPPER` |
| `class-facts-memo-mirror` | `CRATONVM_CLASS_FACTS_MEMO_MIRROR` |
| `class-facts-memo-proxy` | `CRATONVM_CLASS_FACTS_MEMO_PROXY` |
| `class-facts-memo-carriers` | `CRATONVM_CLASS_FACTS_MEMO_CARRIERS` |
| `class-facts-memo-value-hash` | `CRATONVM_CLASS_FACTS_MEMO_VALUE_HASH` |
| `class-facts-memo-isinstance-name` | `CRATONVM_CLASS_FACTS_MEMO_ISINSTANCE_NAME` |
| `class-facts-memo-isinstance-false` | `CRATONVM_CLASS_FACTS_MEMO_ISINSTANCE_FALSE` |
| `class-facts-memo-assignable-false` | `CRATONVM_CLASS_FACTS_MEMO_ASSIGNABLE_FALSE` |
| `jit-mh-compiled-link` | `CRATONVM_JIT_MH_COMPILED_LINK` |
| `jit-mh-compiled-link-linker-by-id` | `CRATONVM_JIT_MH_COMPILED_LINK_LINKER_BY_ID` |
| `jit-mh-compiled-link-user-site` | `CRATONVM_JIT_MH_COMPILED_LINK_USER_SITE` |
| `jit-mh-compiled-link-agree-memo` | `CRATONVM_JIT_MH_COMPILED_LINK_AGREE_MEMO` |
| `jit-mh-compiled-link-pair-memo` | `CRATONVM_JIT_MH_COMPILED_LINK_PAIR_MEMO` |
| `jit-bytecode-only-key` | `CRATONVM_JIT_BYTECODE_ONLY_KEY` |
| `class-shape-jdk-body` | `CRATONVM_CLASS_SHAPE_JDK_BODY` |
| `class-component-jdk-body` | `CRATONVM_CLASS_COMPONENT_JDK_BODY` |
| `profile-fast-call-counts` | `CRATONVM_JIT_PROFILE_FAST_CALL_COUNTS` |
| `ir-seq-charat-hoist` | `CRATONVM_JIT_IR_SEQ_CHARAT_HOIST` |
| `ir-seq-charat-predicate` | `CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE` |
| `ir-seq-charat-predicate-spliced` | `CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE_SPLICED` |
| `ir-seq-charat-field-class-predicate` | `CRATONVM_JIT_IR_SEQ_CHARAT_FIELD_CLASS_PREDICATE` |
| `ir-seq-charat-entry-bound` | `CRATONVM_JIT_IR_SEQ_CHARAT_ENTRY_BOUND` |
| `null-trap-recompile` | `CRATONVM_JIT_NULL_TRAP_RECOMPILE` |
| `tl-set-single-satb` | `CRATONVM_JIT_TL_SET_SINGLE_SATB` |
| `splice-nested-row-past-static-intrinsic` | `CRATONVM_JIT_SPLICE_NESTED_ROW_PAST_STATIC_INTRINSIC` |
| `refers-to-direct` | `CRATONVM_JIT_REFERS_TO_DIRECT` |
| `unsafe-fence-direct` | `CRATONVM_JIT_UNSAFE_FENCE_DIRECT` |
| `unsafe-ref-store-direct` | `CRATONVM_JIT_UNSAFE_REF_STORE_DIRECT` |
| `unsafe-volatile-direct` | `CRATONVM_JIT_UNSAFE_VOLATILE_DIRECT` |
| `trivial-getter-desc-memo` | `CRATONVM_TRIVIAL_GETTER_DESC_MEMO` |
| `field-array-receiver-screen` | `CRATONVM_JIT_FIELD_ARRAY_RECEIVER_SCREEN` |
| `osr-live-stack-retire` | `CRATONVM_JIT_OSR_LIVE_STACK_RETIRE` |
| `ir-splice-putstatic-loop-replay` | `CRATONVM_JIT_IR_SPLICE_PUTSTATIC_LOOP_REPLAY` |
| `ir-loop-intrinsic-call` | `CRATONVM_JIT_IR_LOOP_INTRINSIC_CALL` |
| `ir-splice-putstatic-loop-chain` | `CRATONVM_JIT_IR_SPLICE_PUTSTATIC_LOOP_CHAIN` |
| `gpu-host-callback` | `CRATONVM_GPU_HOST_CALLBACK` |
| `gpu-native-stream-pool` | `CRATONVM_GPU_NATIVE_STREAM_POOL` |
| `ir-lazy-ref-homes` | `CRATONVM_JIT_IR_LAZY_REF_HOMES` |
| `ir-shadow-frame-block-lazy-clear` | `CRATONVM_JIT_IR_SHADOW_FRAME_BLOCK_LAZY_CLEAR` |
| `ir-speculate` | `CRATONVM_JIT_IR_SPECULATE` |

## `CRATONVM_GC`

209 tokens.

| Token | Expands to |
| --- | --- |
| `g1-only-jit-pins` | `CRATONVM_GC_G1_ONLY_JIT_PINS` |
| `queued-finalizables-are-roots` | `CRATONVM_GC_QUEUED_FINALIZABLES_ARE_ROOTS` |
| `jni-indirect-locals` | `CRATONVM_JNI_INDIRECT_LOCALS` |
| `jni-foreign-transitions` | `CRATONVM_JNI_FOREIGN_TRANSITIONS` |
| `jni-native-transitions` | `CRATONVM_JNI_NATIVE_TRANSITIONS` |
| `jni-native-transitions-default` | `CRATONVM_JNI_NATIVE_TRANSITIONS_DEFAULT` |
| `jni-in-native-envelope-first` | `CRATONVM_JNI_IN_NATIVE_ENVELOPE_FIRST` |
| `jni-foreign-tid-publish` | `CRATONVM_JNI_FOREIGN_TID_PUBLISH` |
| `jni-in-native-arrive-no-wait` | `CRATONVM_JNI_IN_NATIVE_ARRIVE_NO_WAIT` |
| `jni-in-native-band-at-pause` | `CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE` |
| `card-metrics` | `CRATONVM_GC_CARD_METRICS` |
| `card-table-only` | `CRATONVM_CARD_TABLE_ONLY` |
| `full-rset-scan` | `CRATONVM_GC_FULL_RSET_SCAN` |
| `verify-rset` | `CRATONVM_GC_VERIFY_RSET` |
| `default-heap-max-mb` | `CRATONVM_DEFAULT_HEAP_MAX_MB` |
| `defrag-promote` | `CRATONVM_NO_DEFRAG_PROMOTE` |
| `exact-refproc-survival` | `CRATONVM_NO_EXACT_REFPROC_SURVIVAL` |
| `referent-identity-screen` | `CRATONVM_NO_REFERENT_IDENTITY_SCREEN` |
| `g1-coverage-pin` | `CRATONVM_G1_COVERAGE_PIN` |
| `g1-pin-empty-publication` | `CRATONVM_G1_PIN_EMPTY_PUBLICATION` |
| `g1-evac-retry` | `CRATONVM_G1_NO_EVAC_RETRY` |
| `g1-parallel-evac` | `CRATONVM_G1_PARALLEL_EVAC` |
| `g1-young-pause-target` | `CRATONVM_G1_YOUNG_PAUSE_TARGET` |
| `g1-humongous-marks` | `CRATONVM_G1_HUMONGOUS_MARKS` |
| `g1-ihop-counts-regions` | `CRATONVM_G1_IHOP_COUNTS_REGIONS` |
| `g1-ihop-gross-growth` | `CRATONVM_G1_IHOP_GROSS_GROWTH` |
| `g1-ihop-backoff` | `CRATONVM_G1_IHOP_BACKOFF` |
| `g1-mark-side-tables` | `CRATONVM_G1_MARK_SIDE_TABLES` |
| `g1-remark-unload-after-keep` | `CRATONVM_G1_REMARK_UNLOAD_AFTER_KEEP` |
| `g1-remark-itl-sweep` | `CRATONVM_G1_REMARK_ITL_SWEEP` |
| `g1-remark-itl-ephemeron` | `CRATONVM_G1_REMARK_ITL_EPHEMERON` |
| `g1-mark-convergence-epoch` | `CRATONVM_G1_MARK_CONVERGENCE_EPOCH` |
| `g1-verify-holders` | `CRATONVM_G1_VERIFY_HOLDERS` |
| `g1-scrub-free` | `CRATONVM_G1_SCRUB_FREE` |
| `g1-verify-forwards-retired` | `CRATONVM_G1_VERIFY_FORWARDS_RETIRED` |
| `g1-evac-copy-watch` | `CRATONVM_G1_EVAC_COPY_WATCH` |
| `g1-ref-write-watch` | `CRATONVM_G1_REF_WRITE_WATCH` |
| `g1-evac-ref-implausible-refuse` | `CRATONVM_G1_EVAC_REF_IMPLAUSIBLE_REFUSE` |
| `g1-cleanup-walk` | `CRATONVM_G1_CLEANUP_WALK` |
| `g1-uncommit` | `CRATONVM_G1_UNCOMMIT` |
| `g1-card-clean` | `CRATONVM_G1_CARD_CLEAN` |
| `g1-card-screen-jit-pinned` | `CRATONVM_G1_CARD_SCREEN_JIT_PINNED` |
| `g1-eden-stripes` | `CRATONVM_G1_EDEN_STRIPES` |
| `g1-workers` | `CRATONVM_G1_WORKERS` |
| `g1-evac-share-census` | `CRATONVM_G1_EVAC_SHARE_CENSUS` |
| `g1-mark-backoff-deadline` | `CRATONVM_G1_MARK_BACKOFF_DEADLINE` |
| `g1-ihop-poll-gate` | `CRATONVM_G1_IHOP_POLL_GATE` |
| `gpu-critical-lease-ms` | `CRATONVM_GPU_CRITICAL_LEASE_MS` |
| `gpu-critical-wait-ms` | `CRATONVM_GPU_CRITICAL_WAIT_MS` |
| `max-inflated-bytes` | `CRATONVM_MAX_INFLATED_BYTES` |
| `oldgen-coalesce` | `CRATONVM_NO_OLDGEN_COALESCE` |
| `oldgen-compact` | `CRATONVM_OLDGEN_COMPACT` |
| `old-trigger-hysteresis` | `CRATONVM_GC_OLD_TRIGGER_HYSTERESIS` |
| `concurrent-first` | `CRATONVM_GC_NO_CONCURRENT_FIRST` |
| `conc-start-percent` | `CRATONVM_GC_CONC_START_PERCENT` |
| `gen-conc-mark-local-stack` | `CRATONVM_GEN_CONC_MARK_NO_LOCAL_STACK` |
| `gen-conc-frozen-eager-scan` | `CRATONVM_GEN_CONC_NO_FROZEN_EAGER_SCAN` |
| `frozen-pages` | `CRATONVM_FROZEN_PAGES` |
| `gen-conc-service-thread` | `CRATONVM_GEN_CONC_SERVICE_THREAD` |
| `gen-conc-remark-refproc-hook` | `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK` |
| `gen-conc-remark-thread-satb-flush` | `CRATONVM_GEN_CONC_REMARK_THREAD_SATB_FLUSH` |
| `gen-satb-old-only` | `CRATONVM_GEN_SATB_OLD_ONLY` |
| `gen-conc-class-unload` | `CRATONVM_GEN_CONC_CLASS_UNLOAD` |
| `softref-hotspot-lru` | `CRATONVM_SOFTREF_HOTSPOT_LRU` |
| `softref-hotspot-free` | `CRATONVM_SOFTREF_HOTSPOT_FREE` |
| `old-give-back` | `CRATONVM_GC_OLD_GIVE_BACK` |
| `old-frag-compact` | `CRATONVM_GC_OLD_FRAG_COMPACT` |
| `old-oom-compact` | `CRATONVM_GC_OLD_OOM_COMPACT` |
| `moving-major-jit-guard` | `CRATONVM_GC_MOVING_MAJOR_JIT_GUARD` |
| `full-gc-true-roots` | `CRATONVM_GC_FULL_GC_TRUE_ROOTS` |
| `gen-young-pin-ledger-term4` | `CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4` |
| `old-pinned-compact` | `CRATONVM_GC_OLD_PINNED_COMPACT` |
| `old-shrink-after-concurrent` | `CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT` |
| `conc-walk-gap-recovery` | `CRATONVM_GC_CONC_WALK_GAP_RECOVERY` |
| `gen-xms-usable-first` | `CRATONVM_GEN_XMS_USABLE_FIRST` |
| `old-interior-decommit` | `CRATONVM_GC_OLD_INTERIOR_DECOMMIT` |
| `old-borrow-young` | `CRATONVM_GC_OLD_BORROW_YOUNG` |
| `old-humongous-top` | `CRATONVM_GC_OLD_HUMONGOUS_TOP` |
| `gen-y2o-live-seed` | `CRATONVM_GEN_Y2O_LIVE_SEED` |
| `gen-young-mirror-defer` | `CRATONVM_GEN_YOUNG_MIRROR_DEFER` |
| `gen-conc-y2o-old-holder-census` | `CRATONVM_GEN_CONC_Y2O_OLD_HOLDER_CENSUS` |
| `gen-precise-root-promote` | `CRATONVM_GEN_PRECISE_ROOT_PROMOTE` |
| `gen-conc-pause-no-precise-root-table` | `CRATONVM_GEN_CONC_PAUSE_NO_PRECISE_ROOT_TABLE` |
| `gen-conc-precedence` | `CRATONVM_GEN_CONC_PRECEDENCE` |
| `gen-conc-preempt-growth-net-of-sweep` | `CRATONVM_GEN_CONC_PREEMPT_GROWTH_NET_OF_SWEEP` |
| `gen-conc-mark-tams-starts` | `CRATONVM_GEN_CONC_MARK_TAMS_STARTS` |
| `gen-conc-inline-start` | `CRATONVM_GEN_CONC_INLINE_START` |
| `overhead-progress` | `CRATONVM_GC_OVERHEAD_PROGRESS` |
| `futile-young-backoff` | `CRATONVM_GC_FUTILE_YOUNG_BACKOFF` |
| `jni-in-native-repeat` | `CRATONVM_JNI_IN_NATIVE_REPEAT` |
| `jni-bulk-array-copy` | `CRATONVM_JNI_BULK_ARRAY_COPY` |
| `old-close-bitmap` | `CRATONVM_GC_OLD_CLOSE_BITMAP` |
| `old-exact-start-class` | `CRATONVM_GC_OLD_EXACT_START_CLASS` |
| `old-compact-forward-index` | `CRATONVM_GC_OLD_COMPACT_FORWARD_INDEX` |
| `old-free-ratio-growth` | `CRATONVM_GC_OLD_FREE_RATIO_GROWTH` |
| `old-shrink-factor` | `CRATONVM_GC_OLD_SHRINK_FACTOR` |
| `old-mark-prefetch` | `CRATONVM_GC_OLD_MARK_PREFETCH` |
| `old-par-adjust` | `CRATONVM_GC_OLD_PAR_ADJUST` |
| `old-par-close` | `CRATONVM_GC_OLD_PAR_CLOSE` |
| `old-par-mark` | `CRATONVM_GC_OLD_PAR_MARK` |
| `old-compact-trust-mark-verify` | `CRATONVM_GC_OLD_COMPACT_TRUST_MARK_VERIFY` |
| `old-trigger-serial` | `CRATONVM_GC_OLD_TRIGGER_SERIAL` |
| `old-capacity-shrink` | `CRATONVM_GC_OLD_CAPACITY_SHRINK` |
| `ref-discover-at-allocation` | `CRATONVM_REF_DISCOVER_AT_ALLOCATION` |
| `ref-notes-per-thread` | `CRATONVM_REF_NOTES_PER_THREAD` |
| `interp-type-map-locals` | `CRATONVM_GC_INTERP_TYPE_MAP_LOCALS` |
| `par-min-bytes` | `CRATONVM_GC_PAR_MIN_BYTES` |
| `par-evac` | `CRATONVM_GC_PAR_EVAC` |
| `par-evac-card-seed` | `CRATONVM_GC_PAR_EVAC_CARD_SEED` |
| `gen-pinned-young-copy-parallel` | `CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL` |
| `system-gc-moving-young` | `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG` |
| `adaptive-tenuring` | `CRATONVM_GC_ADAPTIVE_TENURING` |
| `par-threads` | `CRATONVM_GC_PAR_THREADS` |
| `sync-young-wipe` | `CRATONVM_GC_SYNC_YOUNG_WIPE` |
| `gen-uncommit` | `CRATONVM_GEN_UNCOMMIT` |
| `tlab-filler-skip-zero` | `CRATONVM_TLAB_FILLER_SKIP_ZERO` |
| `tlab-gate-bump-floor` | `CRATONVM_TLAB_GATE_BUMP_FLOOR` |
| `tlab-share-sizer` | `CRATONVM_TLAB_SHARE_SIZER` |
| `gen-zero-once` | `CRATONVM_GEN_ZERO_ONCE` |
| `gen-humongous-zero-unlocked` | `CRATONVM_GEN_HUMONGOUS_ZERO_UNLOCKED` |
| `gen-tlab-tail-sink` | `CRATONVM_GEN_TLAB_TAIL_SINK` |
| `gen-humongous-ref-zero-unlocked` | `CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED` |
| `gen-pinned-young-copy` | `CRATONVM_GEN_PINNED_YOUNG_COPY` |
| `promotion-guard` | `CRATONVM_NO_GC_PROMOTION_GUARD` |
| `promotion-oom-guard-broad` | `CRATONVM_PROMOTION_OOM_GUARD_BROAD` |
| `selective-promote` | `CRATONVM_NO_SELECTIVE_PROMOTE` |
| `stress` | `CRATONVM_GC_STRESS` |
| `sweep-anchor-stride` | `CRATONVM_GC_SWEEP_ANCHOR_STRIDE` |
| `zgc-relocations-prune-amortized` | `CRATONVM_ZGC_RELOCATIONS_PRUNE_AMORTIZED` |
| `zgc-forward-chain-by-epoch` | `CRATONVM_ZGC_FORWARD_CHAIN_BY_EPOCH` |
| `zgc-gen-old-bitmap` | `CRATONVM_ZGC_GEN_OLD_BITMAP` |
| `zgc-gen-bitmap-fallback-major` | `CRATONVM_ZGC_GEN_BITMAP_FALLBACK_MAJOR` |
| `zgc-gen-old-bitmap-verify` | `CRATONVM_ZGC_GEN_OLD_BITMAP_VERIFY` |
| `zgc-parmark-fallback-rescan` | `CRATONVM_ZGC_PARMARK_FALLBACK_RESCAN` |
| `zgc-gen-young-aged-sweep` | `CRATONVM_ZGC_GEN_YOUNG_AGED_SWEEP` |
| `zgc-gen-young-relocate` | `CRATONVM_ZGC_GEN_YOUNG_RELOCATE` |
| `zgc-gen-young-evacuate` | `CRATONVM_ZGC_GEN_YOUNG_EVACUATE` |
| `zgc-gen-slide-old-live` | `CRATONVM_ZGC_GEN_SLIDE_OLD_LIVE` |
| `zgc-nursery-census` | `CRATONVM_ZGC_NURSERY_CENSUS` |
| `zgc-nursery-census-age` | `CRATONVM_ZGC_NURSERY_CENSUS_AGE` |
| `zgc-gen-inline-announce` | `CRATONVM_ZGC_GEN_INLINE_ANNOUNCE` |
| `zgc-gen-element-cards` | `CRATONVM_ZGC_GEN_ELEMENT_CARDS` |
| `zgc-gen-young-complement` | `CRATONVM_ZGC_GEN_YOUNG_COMPLEMENT` |
| `zgc-gen-full-request-major` | `CRATONVM_ZGC_GEN_FULL_REQUEST_MAJOR` |
| `zgc-gen-young-skip-references` | `CRATONVM_ZGC_GEN_YOUNG_SKIP_REFERENCES` |
| `zgc-gen-young-predict-exact` | `CRATONVM_ZGC_GEN_YOUNG_PREDICT_EXACT` |
| `zgc-gen-major-rule-causes` | `CRATONVM_ZGC_GEN_MAJOR_RULE_CAUSES` |
| `zgc-gen-floor-arm-high-old-live` | `CRATONVM_ZGC_GEN_FLOOR_ARM_HIGH_OLD_LIVE` |
| `zgc-gen-system-gc-tenures` | `CRATONVM_ZGC_GEN_SYSTEM_GC_TENURES` |
| `zgc-gen-jit-promotion-floor` | `CRATONVM_ZGC_GEN_JIT_PROMOTION_FLOOR` |
| `zgc-gen-young-pool-live` | `CRATONVM_ZGC_GEN_YOUNG_POOL_LIVE` |
| `zgc-warmup-explicit-door` | `CRATONVM_ZGC_WARMUP_EXPLICIT_DOOR` |
| `zgc-gen-escalate-weak-minor` | `CRATONVM_ZGC_GEN_ESCALATE_WEAK_MINOR` |
| `zgc-gen-empty-minor-needs-promotion` | `CRATONVM_ZGC_GEN_EMPTY_MINOR_NEEDS_PROMOTION` |
| `zgc-gen-nursery-after-warmup` | `CRATONVM_ZGC_GEN_NURSERY_AFTER_WARMUP` |
| `zgc-gen-jmx-young-below-floor` | `CRATONVM_ZGC_GEN_JMX_YOUNG_BELOW_FLOOR` |
| `zgc-gen-jmx-major-young` | `CRATONVM_ZGC_GEN_JMX_MAJOR_YOUNG` |
| `zgc-timer-not-warmup` | `CRATONVM_ZGC_TIMER_NOT_WARMUP` |
| `zgc-tick-withdraw-own-bit` | `CRATONVM_ZGC_TICK_WITHDRAW_OWN_BIT` |
| `zgc-tick-stop-aborts` | `CRATONVM_ZGC_TICK_STOP_ABORTS` |
| `zgc-collection-interval-only` | `CRATONVM_ZGC_COLLECTION_INTERVAL_ONLY` |
| `zgc-timer-skips-stall-cycles` | `CRATONVM_ZGC_TIMER_SKIPS_STALL_CYCLES` |
| `zgc-stall-mark-reaches-joined-pause` | `CRATONVM_ZGC_STALL_MARK_REACHES_JOINED_PAUSE` |
| `zgc-interval-only-occupancy-doors-stall` | `CRATONVM_ZGC_INTERVAL_ONLY_OCCUPANCY_DOORS_STALL` |
| `zgc-tick-withdraw-keeps-poll-proactive` | `CRATONVM_ZGC_TICK_WITHDRAW_KEEPS_POLL_PROACTIVE` |
| `zgc-gen-high-young-live` | `CRATONVM_ZGC_GEN_HIGH_YOUNG_LIVE` |
| `zgc-census` | `CRATONVM_ZGC_CENSUS` |
| `zgc-census-access` | `CRATONVM_ZGC_CENSUS_ACCESS` |
| `zgc-mark-ref-disps` | `CRATONVM_ZGC_MARK_REF_DISPS` |
| `zgc-mark-layout-memo` | `CRATONVM_ZGC_MARK_LAYOUT_MEMO` |
| `zgc-mark-test-first` | `CRATONVM_ZGC_MARK_TEST_FIRST` |
| `zgc-mark-one-decode` | `CRATONVM_ZGC_MARK_ONE_DECODE` |
| `zgc-tlab` | `CRATONVM_ZGC_TLAB` |
| `zgc-color-flip` | `CRATONVM_ZGC_COLOR_FLIP` |
| `zgc-footprint-trim` | `CRATONVM_ZGC_FOOTPRINT_TRIM` |
| `zgc-reset-accounting` | `CRATONVM_ZGC_RESET_ACCOUNTING` |
| `zgc-define-mirror-reserved-every-definer` | `CRATONVM_ZGC_DEFINE_MIRROR_RESERVED_EVERY_DEFINER` |
| `zgc-unload-class-name-memo` | `CRATONVM_ZGC_UNLOAD_CLASS_NAME_MEMO` |
| `zgc-relocate-size-table` | `CRATONVM_ZGC_RELOCATE_SIZE_TABLE` |
| `mimalloc-lazy-arena-commit` | `CRATONVM_MIMALLOC_LAZY_ARENA_COMMIT` |
| `mimalloc-sparse-block-mmap` | `CRATONVM_MIMALLOC_SPARSE_BLOCK_MMAP` |
| `mimalloc-sparse-block-min-kb` | `CRATONVM_MIMALLOC_SPARSE_BLOCK_MIN_KB` |
| `mimalloc-sparse-block-boot-count` | `CRATONVM_MIMALLOC_SPARSE_BLOCK_BOOT_COUNT` |
| `mimalloc-arena-max-object-kb` | `CRATONVM_MIMALLOC_ARENA_MAX_OBJECT_KB` |
| `zgc-sweep-size-hints` | `CRATONVM_ZGC_SWEEP_SIZE_HINTS` |
| `zgc-selective-relocate-hole-kib` | `CRATONVM_ZGC_SELECTIVE_RELOCATE_HOLE_KIB` |
| `zgc-selective-relocate-frag-pct` | `CRATONVM_ZGC_SELECTIVE_RELOCATE_FRAG_PCT` |
| `zgc-slide-pin-by-base` | `CRATONVM_ZGC_SLIDE_PIN_BY_BASE` |
| `zgc-relocation-set-books-final` | `CRATONVM_ZGC_RELOCATION_SET_BOOKS_FINAL` |
| `zgc-target-exemption-after-pins` | `CRATONVM_ZGC_TARGET_EXEMPTION_AFTER_PINS` |
| `zgc-pause-gcinfo-no-usage` | `CRATONVM_ZGC_PAUSE_GCINFO_NO_USAGE` |
| `zgc-pool-peak-at-gc` | `CRATONVM_ZGC_POOL_PEAK_AT_GC` |
| `zgc-hotspot-pool-init` | `CRATONVM_ZGC_HOTSPOT_POOL_INIT` |
| `zgc-jmx-metaspace-pool` | `CRATONVM_ZGC_JMX_METASPACE_POOL` |
| `zgc-jmx-hotspot-non-heap-pools` | `CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS` |
| `zgc-code-heap-init-hotspot` | `CRATONVM_ZGC_CODE_HEAP_INIT_HOTSPOT` |
| `zgc-gcinfo-code-heap-edges` | `CRATONVM_ZGC_GCINFO_CODE_HEAP_EDGES` |
| `zgc-metaspace-first-cycles` | `CRATONVM_ZGC_METASPACE_FIRST_CYCLES` |
| `zgc-metaspace-threshold` | `CRATONVM_ZGC_METASPACE_THRESHOLD` |
| `zgc-metaspace-peak-at-gc` | `CRATONVM_ZGC_METASPACE_PEAK_AT_GC` |
| `zgc-threshold-set-drain` | `CRATONVM_ZGC_THRESHOLD_SET_DRAIN` |
| `zgc-remap-in-mark` | `CRATONVM_ZGC_REMAP_IN_MARK` |
| `zgc-region-relocate` | `CRATONVM_ZGC_REGION_RELOCATE` |
| `zgc-sweep-size-memo` | `CRATONVM_ZGC_SWEEP_SIZE_MEMO` |
| `zgc-conc-close-forgets-pause-budget` | `CRATONVM_ZGC_CONC_CLOSE_FORGETS_PAUSE_BUDGET` |
| `zgc-gen-minor-timer` | `CRATONVM_ZGC_GEN_MINOR_TIMER` |
| `handshake-at-helper-entry` | `CRATONVM_GC_HANDSHAKE_AT_HELPER_ENTRY` |
| `gen-young-mirror-root-when-promotable` | `CRATONVM_GEN_YOUNG_MIRROR_ROOT_WHEN_PROMOTABLE` |
| `gen-young-mirror-defer-moving-follows` | `CRATONVM_GEN_YOUNG_MIRROR_DEFER_MOVING_FOLLOWS` |

## `CRATONVM_REAL`

30 tokens.

| Token | Expands to |
| --- | --- |
| `props-unrooted-receivers` | `CRATONVM_PROPS_UNROOTED_RECEIVERS` |
| `bytebuffer-intrinsic` | `CRATONVM_BYTEBUFFER_INTRINSIC` |
| `itr-bytecode` | `CRATONVM_ITR_BYTECODE` |
| `agroal` | `CRATONVM_REAL_AGROAL / CRATONVM_SYNTHETIC_AGROAL` |
| `annotations` | `CRATONVM_REAL_ANNOTATIONS / CRATONVM_SYNTHETIC_ANNOTATIONS` |
| `aqs` | `CRATONVM_REAL_AQS / CRATONVM_SYNTHETIC_AQS` |
| `dsa` | `CRATONVM_SYNTHETIC_DSA` |
| `ec` | `CRATONVM_SYNTHETIC_EC` |
| `eqe` | `CRATONVM_SYNTHETIC_EQE` |
| `filewriter` | `CRATONVM_SYNTHETIC_FILEWRITER` |
| `forkjoinpool` | `CRATONVM_REAL_FORKJOINPOOL / CRATONVM_SYNTHETIC_FORKJOINPOOL` |
| `jca` | `CRATONVM_REAL_JCA` |
| `memoryusage-tostring` | `CRATONVM_SYNTHETIC_MEMORYUSAGE_TOSTRING` |
| `msc-real-start` | `CRATONVM_MSC_REAL_START` |
| `mxbean-mapping` | `CRATONVM_SYNTHETIC_MXBEAN_MAPPING` |
| `net-sockets` | `CRATONVM_REAL_NET_SOCKETS / CRATONVM_SYNTHETIC_NET_SOCKETS` |
| `netty-tcnative` | `CRATONVM_SYNTHETIC_NETTY_TCNATIVE` |
| `pqc` | `CRATONVM_SYNTHETIC_PQC` |
| `proxy` | `CRATONVM_REAL_PROXY` |
| `proxy-strict` | `CRATONVM_REAL_PROXY_STRICT` |
| `proxy-super` | `CRATONVM_REAL_PROXY_SUPER` |
| `quarkus-arc` | `CRATONVM_SYNTHETIC_QUARKUS_ARC` |
| `quarkus-start` | `CRATONVM_REAL_QUARKUS_START / CRATONVM_SYNTHETIC_QUARKUS_START` |
| `raf` | `CRATONVM_SYNTHETIC_RAF` |
| `rsa` | `CRATONVM_SYNTHETIC_RSA` |
| `stax-factory` | `CRATONVM_REAL_STAX_FACTORY` |
| `stubs` | `CRATONVM_NO_STUBS` |
| `use-wildfly-reflect-shim` | `CRATONVM_USE_WILDFLY_REFLECT_SHIM` |
| `use-wildfly-synth-bytecode` | `CRATONVM_USE_WILDFLY_SYNTH_BYTECODE` |
| `vertx` | `CRATONVM_REAL_VERTX / CRATONVM_SYNTHETIC_VERTX` |

## `CRATONVM_LOADER`

85 tokens.

| Token | Expands to |
| --- | --- |
| `nested-jar-shared-outer` | `CRATONVM_NESTED_JAR_SHARED_OUTER` |
| `nested-jar-bytes-directory` | `CRATONVM_NESTED_JAR_BYTES_DIRECTORY` |
| `nested-jar-outer-handle-reuse` | `CRATONVM_NESTED_JAR_OUTER_HANDLE_REUSE` |
| `nested-jar-class-path-outer` | `CRATONVM_NESTED_JAR_CLASS_PATH_OUTER` |
| `nested-jar-bytes-index` | `CRATONVM_NESTED_JAR_BYTES_INDEX` |
| `jar-loader-bytes-index` | `CRATONVM_JAR_LOADER_BYTES_INDEX` |
| `nested-jar-pooled-stat` | `CRATONVM_NESTED_JAR_POOLED_STAT` |
| `class-mirror-one-loader-lookup` | `CRATONVM_CLASS_MIRROR_ONE_LOADER_LOOKUP` |
| `define-boot-maps-at-link` | `CRATONVM_DEFINE_BOOT_MAPS_AT_LINK` |
| `link-state-one-write` | `CRATONVM_LINK_STATE_ONE_WRITE` |
| `define-prepass-object-only` | `CRATONVM_DEFINE_PREPASS_OBJECT_ONLY` |
| `define-vtable-inherit-template` | `CRATONVM_DEFINE_VTABLE_INHERIT_TEMPLATE` |
| `user-loader-resource-bytecode-compiled` | `CRATONVM_USER_LOADER_RESOURCE_BYTECODE_COMPILED` |
| `loader-dictionary-before-drive` | `CRATONVM_LOADER_DICTIONARY_BEFORE_DRIVE` |
| `loader-dictionary-one-door` | `CRATONVM_LOADER_DICTIONARY_ONE_DOOR` |
| `loader-request-door` | `CRATONVM_LOADER_REQUEST_DOOR` |
| `dup-name-field-gate` | `CRATONVM_LOADER_NO_DUP_NAME_FIELD_GATE` |
| `ann-proxy-latch` | `CRATONVM_LOADER_NO_ANN_PROXY_LATCH` |
| `resolution-failure-record` | `CRATONVM_LOADER_NO_RESOLUTION_FAILURE_RECORD` |
| `subtype-display` | `CRATONVM_LOADER_NO_SUBTYPE_DISPLAY` |
| `allow-jsr-ret` | `CRATONVM_ALLOW_JSR_RET` |
| `verify-receiver-assignable` | `CRATONVM_VERIFY_RECEIVER_ASSIGNABLE` |
| `verify-handler-frames` | `CRATONVM_VERIFY_HANDLER_FRAMES` |
| `verify-user-loader-classes` | `CRATONVM_VERIFY_USER_LOADER_CLASSES` |
| `verify-user-loader-classes-at-link` | `CRATONVM_VERIFY_USER_LOADER_CLASSES_AT_LINK` |
| `user-pass3-records-link-loads` | `CRATONVM_USER_PASS3_RECORDS_LINK_LOADS` |
| `verify-control-rules` | `CRATONVM_VERIFY_CONTROL_RULES` |
| `verify-subroutines` | `CRATONVM_VERIFY_SUBROUTINES` |
| `verify-this-uninit-flags` | `CRATONVM_VERIFY_THIS_UNINIT_FLAGS` |
| `verify-major50-failover` | `CRATONVM_VERIFY_MAJOR50_FAILOVER` |
| `enforce-native-shadow` | `CRATONVM_ENFORCE_NATIVE_SHADOW` |
| `unretire-native-shadow` | `CRATONVM_UNRETIRE_NATIVE_SHADOW` |
| `pure-bytecode` | `CRATONVM_PURE_BYTECODE` |
| `audit-yield-intrinsic` | `CRATONVM_AUDIT_YIELD_INTRINSIC` |
| `lever-keep` | `CRATONVM_LEVER_KEEP` |
| `lever-drop` | `CRATONVM_LEVER_DROP` |
| `compat-lhm-reversed-live-views` | `CRATONVM_COMPAT_LHM_REVERSED_LIVE_VIEWS` |
| `compat-immutable-of-jdk-mint` | `CRATONVM_COMPAT_IMMUTABLE_OF_JDK_MINT` |
| `compat-setn-iteration-order` | `CRATONVM_COMPAT_SETN_ITERATION_ORDER` |
| `compat-unmod-iterator-jdk` | `CRATONVM_COMPAT_UNMOD_ITERATOR_JDK` |
| `compat-hashmap-serialized-threshold` | `CRATONVM_COMPAT_HASHMAP_SERIALIZED_THRESHOLD` |
| `compat-put-all-null-key` | `CRATONVM_COMPAT_PUT_ALL_NULL_KEY` |
| `compat-al-add-all-empty-modcount` | `CRATONVM_COMPAT_AL_ADD_ALL_EMPTY_MODCOUNT` |
| `compat-al-listitr-comod` | `CRATONVM_COMPAT_AL_LISTITR_COMOD` |
| `compat-hashmap-copy-presize` | `CRATONVM_COMPAT_HASHMAP_COPY_PRESIZE` |
| `compat-null-collection-message` | `CRATONVM_COMPAT_NULL_COLLECTION_MESSAGE` |
| `compat-chm-jdk25-sizing` | `CRATONVM_COMPAT_CHM_JDK25_SIZING` |
| `compat-lhs-reversed-real-view` | `CRATONVM_COMPAT_LHS_REVERSED_REAL_VIEW` |
| `cf-delegating-yield` | `CRATONVM_CF_DELEGATING_YIELD` |
| `aware-resolution` | `CRATONVM_LOADER_AWARE_RESOLUTION` |
| `boot-module-registry` | `CRATONVM_BOOT_MODULE_REGISTRY` |
| `classpath-jar-unnamed-module` | `CRATONVM_CLASSPATH_JAR_UNNAMED_MODULE` |
| `fwd-resolve-strict` | `CRATONVM_FWD_RESOLVE_STRICT` |
| `jar-mmap` | `CRATONVM_DISABLE_JAR_MMAP` |
| `class-path-live-jar-no-reread` | `CRATONVM_CLASS_PATH_LIVE_JAR_NO_REREAD` |
| `jmod-lazy-index` | `CRATONVM_JMOD_LAZY_INDEX` |
| `jar-lazy-index` | `CRATONVM_JAR_LAZY_INDEX` |
| `jimage-package-index` | `CRATONVM_JIMAGE_PACKAGE_INDEX` |
| `jimage-skip-directory-records` | `CRATONVM_JIMAGE_SKIP_DIRECTORY_RECORDS` |
| `class-code-compact` | `CRATONVM_CLASS_CODE_COMPACT` |
| `boot-class-bytes-refetch` | `CRATONVM_BOOT_CLASS_BYTES_REFETCH` |
| `boot-image-class-bytes` | `CRATONVM_BOOT_IMAGE_CLASS_BYTES` |
| `lenient-clinit` | `CRATONVM_LENIENT_CLINIT` |
| `longrewrite-loose` | `CRATONVM_LONGREWRITE_LOOSE` |
| `parent-chain` | `CRATONVM_LOADER_PARENT_CHAIN` |
| `resolve-cache-cap` | `CRATONVM_RESOLVE_CACHE_CAP` |
| `stub-delegation` | `CRATONVM_CL_STUB_DELEGATION` |
| `unload` | `CRATONVM_LOADER_UNLOAD` |
| `isolated-loader-dictionary` | `CRATONVM_ISOLATED_LOADER_DICTIONARY` |
| `define-add-class-hinted` | `CRATONVM_DEFINE_ADD_CLASS_HINTED` |
| `loader-find-loaded-no-standin` | `CRATONVM_LOADER_FIND_LOADED_NO_STANDIN` |
| `clv-sub-fast-key` | `CRATONVM_CLV_SUB_FAST_KEY` |
| `verify-handler-catch-memo` | `CRATONVM_VERIFY_HANDLER_CATCH_MEMO` |
| `link-type-map-walk-loads-only` | `CRATONVM_LINK_TYPE_MAP_WALK_LOADS_ONLY` |
| `verify-initializers-first` | `CRATONVM_VERIFY_INITIALIZERS_FIRST` |
| `loader-label-parent-memo` | `CRATONVM_LOADER_LABEL_PARENT_MEMO` |
| `verify-type-map-buffer-reuse` | `CRATONVM_VERIFY_TYPE_MAP_BUFFER_REUSE` |
| `verify-subclass-memo` | `CRATONVM_VERIFY_SUBCLASS_MEMO` |
| `verify-name-answer-memo` | `CRATONVM_VERIFY_NAME_ANSWER_MEMO` |
| `loader-link-verdict-memo` | `CRATONVM_LOADER_LINK_VERDICT_MEMO` |
| `loader-load-class-override-memo` | `CRATONVM_LOADER_LOAD_CLASS_OVERRIDE_MEMO` |
| `class-by-name-no-alloc` | `CRATONVM_CLASS_BY_NAME_NO_ALLOC` |
| `real-builtin-loaders` | `CRATONVM_REAL_BUILTIN_LOADERS` |
| `method-type-no-loader-resolves-in-vm` | `CRATONVM_METHOD_TYPE_NO_LOADER_RESOLVES_IN_VM` |
| `eager-bootstrap-all` | `CRATONVM_EAGER_BOOTSTRAP_ALL` |

## `CRATONVM_IO`

15 tokens.

| Token | Expands to |
| --- | --- |
| `inflater-reuse-buffers` | `CRATONVM_INFLATER_REUSE_BUFFERS` |
| `zip-cen-accessor-intrinsics` | `CRATONVM_ZIP_CEN_ACCESSOR_INTRINSICS` |
| `zip-entry-miss-intrinsic` | `CRATONVM_ZIP_ENTRY_MISS_INTRINSIC` |
| `jimage-direct-map` | `CRATONVM_JIMAGE_DIRECT_MAP` |
| `canon-openfile` | `CRATONVM_CANON_OPENFILE` |
| `http-max-body` | `CRATONVM_HTTP_MAX_BODY` |
| `netty-queue-bridge` | `CRATONVM_NETTY_QUEUE_BRIDGE` |
| `resolve-outbound-host` | `CRATONVM_RESOLVE_OUTBOUND_HOST` |
| `select-max-block-ms` | `CRATONVM_SELECT_MAX_BLOCK_MS` |
| `selector-connect-probe` | `CRATONVM_NO_SELECTOR_CONNECT_PROBE` |
| `net-jdk-backlog` | `CRATONVM_NET_JDK_BACKLOG` |
| `jar-archive-cache-cap` | `CRATONVM_JAR_ARCHIVE_CACHE_CAP` |
| `httpsrv-keepalive` | `CRATONVM_HTTPSRV_KEEPALIVE` |
| `socket-capture` | `CRATONVM_SOCKET_CAPTURE` |
| `zip-max-entry-bytes` | `CRATONVM_ZIP_MAX_ENTRY_BYTES` |

## `CRATONVM_THREADS`

55 tokens.

| Token | Expands to |
| --- | --- |
| `vthread-slice-on-carrier` | `CRATONVM_VTHREAD_SLICE_ON_CARRIER` |
| `vthread-slice-on-carrier-compiled` | `CRATONVM_VTHREAD_SLICE_ON_CARRIER_COMPILED` |
| `vthread-lean-registry` | `CRATONVM_VTHREAD_LEAN_REGISTRY` |
| `vthread-carrier-sleeps` | `CRATONVM_VTHREAD_CARRIER_SLEEPS` |
| `park-lazy-stw-wake` | `CRATONVM_PARK_LAZY_STW_WAKE` |
| `park-deposit-frozen-band` | `CRATONVM_PARK_DEPOSIT_FROZEN_BAND` |
| `park-lazy-suspend-aware` | `CRATONVM_PARK_LAZY_SUSPEND_AWARE` |
| `park-atomic-permit` | `CRATONVM_PARK_ATOMIC_PERMIT` |
| `unpark-eetop` | `CRATONVM_UNPARK_EETOP` |
| `park-wake-after-unlock` | `CRATONVM_PARK_WAKE_AFTER_UNLOCK` |
| `park-untimed-event` | `CRATONVM_PARK_UNTIMED_EVENT` |
| `park-event-signal-after-unlock` | `CRATONVM_PARK_EVENT_SIGNAL_AFTER_UNLOCK` |
| `park-event-lockless-wake` | `CRATONVM_PARK_EVENT_LOCKLESS_WAKE` |
| `sleep-high-resolution-interval` | `CRATONVM_SLEEP_HIGH_RESOLUTION_INTERVAL` |
| `park-lazy-spin-ns` | `CRATONVM_PARK_LAZY_SPIN_NS` |
| `thread-start-one-publish` | `CRATONVM_THREAD_START_ONE_PUBLISH` |
| `jmx-owned-synchronizers` | `CRATONVM_JMX_OWNED_SYNCHRONIZERS` |
| `jmx-caller-held-method-monitor-depths` | `CRATONVM_JMX_CALLER_HELD_METHOD_MONITOR_DEPTHS` |
| `jmx-running-locks-at-trace` | `CRATONVM_JMX_RUNNING_LOCKS_AT_TRACE` |
| `monitor-fastpath` | `CRATONVM_MONITOR_FASTPATH` |
| `monitor-spin-handover-abort` | `CRATONVM_MONITOR_SPIN_HANDOVER_ABORT` |
| `monitor-enter-spin` | `CRATONVM_MONITOR_ENTER_SPIN` |
| `monitor-spin-after-wake` | `CRATONVM_MONITOR_SPIN_AFTER_WAKE` |
| `monitor-thin-notify` | `CRATONVM_MONITOR_THIN_NOTIFY` |
| `monitor-index-prune` | `CRATONVM_MONITOR_INDEX_PRUNE` |
| `lock-key-shards` | `CRATONVM_LOCK_KEY_SHARDS` |
| `tl-intrinsic` | `CRATONVM_TL_INTRINSIC` |
| `tl-real-fast` | `CRATONVM_TL_REAL_FAST` |
| `refers-to-leaf` | `CRATONVM_REFERS_TO_LEAF` |
| `assert-single-os-thread` | `CRATONVM_ASSERT_SINGLE_OS_THREAD` |
| `async-handoff-sleep-floor-ms` | `CRATONVM_ASYNC_HANDOFF_SLEEP_FLOOR_MS` |
| `async-submit-grace-ms` | `CRATONVM_ASYNC_SUBMIT_GRACE_MS` |
| `async-worker-sleep-floor-ms` | `CRATONVM_ASYNC_WORKER_SLEEP_FLOOR_MS` |
| `await-shortcircuit` | `CRATONVM_AWAIT_NO_SHORTCIRCUIT` |
| `default-watchdog` | `CRATONVM_DISABLE_DEFAULT_WATCHDOG` |
| `default-watchdog-sec` | `CRATONVM_DEFAULT_WATCHDOG_SEC` |
| `thread-containers` | `CRATONVM_THREAD_CONTAINERS` |
| `eqe-sync-execute` | `CRATONVM_EQE_SYNC_EXECUTE` |
| `exec-depth-ceiling` | `CRATONVM_EXEC_DEPTH_CEILING` |
| `fjp-eager-fork` | `CRATONVM_FJP_EAGER_FORK` |
| `inherit-thread-ccl` | `CRATONVM_INHERIT_THREAD_CCL` |
| `inherit-tl-workaround` | `CRATONVM_INHERIT_TL_WORKAROUND` |
| `lock-order-check` | `CRATONVM_LOCK_ORDER_CHECK` |
| `shutdown-hook-timeout-ms` | `CRATONVM_SHUTDOWN_HOOK_TIMEOUT_MS` |
| `stress-thread-states` | `CRATONVM_STRESS_THREAD_STATES` |
| `striped-counters` | `CRATONVM_STRIPED_COUNTERS_OFF` |
| `thread-start-grace-ms` | `CRATONVM_THREAD_START_GRACE_MS` |
| `wait-spurious-ms` | `CRATONVM_WAIT_SPURIOUS_MS` |
| `monitor-pending-notify` | `CRATONVM_MONITOR_PENDING_NOTIFY` |
| `monitor-heir` | `CRATONVM_MONITOR_HEIR` |
| `monitor-heir-ordered` | `CRATONVM_MONITOR_HEIR_ORDERED` |
| `monitor-heir-signal` | `CRATONVM_MONITOR_HEIR_SIGNAL` |
| `win-hires-park` | `CRATONVM_WIN_HIRES_PARK` |
| `init-finalizer-tail` | `CRATONVM_INIT_FINALIZER_TAIL` |
| `blocking-leave-key-reuses-buffers` | `CRATONVM_BLOCKING_LEAVE_KEY_REUSES_BUFFERS` |

## `CRATONVM_SECURITY`

14 tokens.

| Token | Expands to |
| --- | --- |
| `aot-hmac-key` | `CRATONVM_AOT_HMAC_KEY` |
| `jca-lenient-getinstance` | `CRATONVM_JCA_LENIENT_GETINSTANCE` |
| `block-private-nets` | `CRATONVM_BLOCK_PRIVATE_NETS` |
| `capability-grants` | `CRATONVM_CAPABILITY_GRANTS` |
| `capability-log` | `CRATONVM_CAPABILITY_LOG` |
| `capability-mode` | `CRATONVM_CAPABILITY_MODE` |
| `confine-io` | `CRATONVM_CONFINE_IO` |
| `harden-manifest-classpath` | `CRATONVM_HARDEN_MANIFEST_CLASSPATH` |
| `noncrypto-sslengine` | `CRATONVM_ALLOW_NONCRYPTO_SSLENGINE` |
| `reflect-export-gate` | `CRATONVM_REFLECT_NO_EXPORT_GATE` |
| `require-policy` | `CRATONVM_REQUIRE_POLICY` |
| `trust-pem` | `CRATONVM_TRUST_PEM` |
| `tls-openssl-client` | `CRATONVM_TLS_OPENSSL_CLIENT` |
| `untrusted-code` | `CRATONVM_UNTRUSTED_CODE` |

## `CRATONVM_COMPAT`

50 tokens.

| Token | Expands to |
| --- | --- |
| `module-access-export-one-question` | `CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION` |
| `compat-initiating-records` | `CRATONVM_COMPAT_INITIATING_RECORDS` |
| `compat-parallel-loaders` | `CRATONVM_COMPAT_PARALLEL_LOADERS` |
| `indy-checks-every-mode` | `CRATONVM_INDY_CHECKS_EVERY_MODE` |
| `selection-checks-every-mode` | `CRATONVM_SELECTION_CHECKS_EVERY_MODE` |
| `mh-combinator-checks-every-mode` | `CRATONVM_MH_COMBINATOR_CHECKS_EVERY_MODE` |
| `ldc-handle-access-every-mode` | `CRATONVM_LDC_HANDLE_ACCESS_EVERY_MODE` |
| `compat-loader-constraints` | `CRATONVM_COMPAT_LOADER_CONSTRAINTS` |
| `compat-loader-throws` | `CRATONVM_COMPAT_LOADER_THROWS` |
| `compat-global-name-ask` | `CRATONVM_COMPAT_GLOBAL_NAME_ASK` |
| `compat-url-app-handler` | `CRATONVM_COMPAT_URL_APP_HANDLER` |
| `compat-named-packages` | `CRATONVM_COMPAT_NAMED_PACKAGES` |
| `compat-module-messages` | `CRATONVM_COMPAT_MODULE_MESSAGES` |
| `compat-jdk-immutables` | `CRATONVM_COMPAT_JDK_IMMUTABLES` |
| `module-export-all-by-identity` | `CRATONVM_MODULE_EXPORT_ALL_BY_IDENTITY` |
| `compat-jdk-lock-support` | `CRATONVM_COMPAT_JDK_LOCK_SUPPORT` |
| `compat-jdk-stamped-lock` | `CRATONVM_COMPAT_JDK_STAMPED_LOCK` |
| `compat-jdk-count-down-latch` | `CRATONVM_COMPAT_JDK_COUNT_DOWN_LATCH` |
| `compat-jdk-aqls` | `CRATONVM_COMPAT_JDK_AQLS` |
| `compat-jdk-copy-on-write` | `CRATONVM_COMPAT_JDK_COPY_ON_WRITE` |
| `compat-jdk-copy-on-write-set` | `CRATONVM_COMPAT_JDK_COPY_ON_WRITE_SET` |
| `compat-jdk-priority-blocking-queue` | `CRATONVM_COMPAT_JDK_PRIORITY_BLOCKING_QUEUE` |
| `compat-jdk-concurrent-hash-map` | `CRATONVM_COMPAT_JDK_CONCURRENT_HASH_MAP` |
| `compat-jdk-process` | `CRATONVM_COMPAT_JDK_PROCESS` |
| `compat-jdk-completable-future` | `CRATONVM_COMPAT_JDK_COMPLETABLE_FUTURE` |
| `compat-jdk-thread-pools` | `CRATONVM_COMPAT_JDK_THREAD_POOLS` |
| `compat-jdk-hashtable` | `CRATONVM_COMPAT_JDK_HASHTABLE` |
| `compat-jdk-logging` | `CRATONVM_COMPAT_JDK_LOGGING` |
| `compat-jdk-string-get-bytes` | `CRATONVM_COMPAT_JDK_STRING_GET_BYTES` |
| `compat-jdk-memory-usage` | `CRATONVM_COMPAT_JDK_MEMORY_USAGE` |
| `compat-jdk-management-beans` | `CRATONVM_COMPAT_JDK_MANAGEMENT_BEANS` |
| `compat-jdk-type-variable-impl` | `CRATONVM_COMPAT_JDK_TYPE_VARIABLE_IMPL` |
| `compat-jdk-priority-queue` | `CRATONVM_COMPAT_JDK_PRIORITY_QUEUE` |
| `compat-align` | `CRATONVM_COMPAT_ALIGN` |
| `perf-attach-foreign` | `CRATONVM_PERF_ATTACH_FOREIGN` |
| `perf-data-in-memory-fallback` | `CRATONVM_PERF_DATA_IN_MEMORY_FALLBACK` |
| `perf-attach-missing-throws` | `CRATONVM_PERF_ATTACH_MISSING_THROWS` |
| `perf-hrt-qpc-frequency` | `CRATONVM_PERF_HRT_QPC_FREQUENCY` |
| `generic-reflect-memo` | `CRATONVM_GENERIC_REFLECT_MEMO` |
| `reflect-new-array-one-resolve` | `CRATONVM_REFLECT_NEW_ARRAY_ONE_RESOLVE` |
| `jboss-mp-root` | `CRATONVM_JBOSS_MP_ROOT` |
| `stack-trace-module-prefix` | `CRATONVM_STACK_TRACE_MODULE_PREFIX` |
| `stack-trace-element-exact-class` | `CRATONVM_STACK_TRACE_ELEMENT_EXACT_CLASS` |
| `stack-trace-element-full-origin` | `CRATONVM_STACK_TRACE_ELEMENT_FULL_ORIGIN` |
| `stack-trace-binary-names` | `CRATONVM_STACK_TRACE_BINARY_NAMES` |
| `throwable-thread-run-nested-tasks` | `CRATONVM_THROWABLE_THREAD_RUN_NESTED_TASKS` |
| `thread-run-thread-declared-fields` | `CRATONVM_THREAD_RUN_THREAD_DECLARED_FIELDS` |
| `thread-layout-cache` | `CRATONVM_THREAD_LAYOUT_CACHE` |
| `stack-trace-element-origin-memo` | `CRATONVM_STACK_TRACE_ELEMENT_ORIGIN_MEMO` |
| `lookup-in-loads-outer` | `CRATONVM_LOOKUP_IN_LOADS_OUTER` |

## `CRATONVM_TEST`

3 tokens.

| Token | Expands to |
| --- | --- |
| `jdk` | `CRATONVM_TEST_JDK` |
| `force-win-build` | `CRATONVM_FORCE_WIN_BUILD` |
| `segv` | `CRATONVM_TEST_SEGV` |

