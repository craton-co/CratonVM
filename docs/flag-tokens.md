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

## `CRATONVM_DBG`

506 tokens.

| Token | Expands to |
| --- | --- |
| `a2` | `CRATONVM_DBG_A2` |
| `a5-census` | `CRATONVM_DBG_A5_CENSUS` |
| `ffm` | `CRATONVM_DBG_FFM` |
| `sweep-liveness` | `CRATONVM_DBG_SWEEP_LIVENESS` |
| `callee-deopt` | `CRATONVM_DBG_CALLEE_DEOPT` |
| `layout-alias` | `CRATONVM_DBG_LAYOUT_ALIAS` |
| `check-override` | `CRATONVM_DBG_CHECK_OVERRIDE` |
| `dial-doors` | `CRATONVM_DBG_DIAL_DOORS` |
| `direct-memory` | `CRATONVM_DBG_DM` |
| `dupx-trace` | `CRATONVM_DBG_DUPX_TRACE` |
| `watch-pun` | `CRATONVM_DBG_WATCH_PUN` |
| `dropped-putfield` | `CRATONVM_DBG_DROPPED_PUTFIELD` |
| `read0-latency` | `CRATONVM_DBG_READ0LAT` |
| `refdisc` | `CRATONVM_DBG_REFDISC` |
| `site-alias` | `CRATONVM_DBG_SITE_ALIAS` |
| `sp-ic-sites` | `CRATONVM_DBG_SP_IC_SITES` |
| `stub-yield` | `CRATONVM_DBG_STUB_YIELD` |
| `access` | `CRATONVM_DBG_ACCESS` |
| `active-profiles-identity-trace` | `CRATONVM_ACTIVE_PROFILES_IDENTITY_TRACE` |
| `aio` | `CRATONVM_DBG_AIO` |
| `aio-inline` | `CRATONVM_DBG_AIO_INLINE` |
| `aioobe` | `CRATONVM_DBG_AIOOBE` |
| `aioobe2` | `CRATONVM_DBG_AIOOBE2` |
| `aioobe3` | `CRATONVM_DBG_AIOOBE3` |
| `altrace` | `CRATONVM_DBG_ALTRACE` |
| `ann-proxy-dispatch-trace` | `CRATONVM_ANN_PROXY_DISPATCH_TRACE` |
| `ann-proxy-prof` | `CRATONVM_DBG_ANN_PROXY_PROF` |
| `ann-trace` | `CRATONVM_ANN_TRACE` |
| `annproxy-wrap` | `CRATONVM_DBG_ANNPROXY_WRAP` |
| `anonalloc` | `CRATONVM_DBG_ANONALLOC` |
| `aqs-trace` | `CRATONVM_DBG_AQS_TRACE` |
| `args` | `CRATONVM_DBG_ARGS` |
| `arraycopy` | `CRATONVM_DBG_ARRAYCOPY` |
| `arrlen` | `CRATONVM_DBG_ARRLEN` |
| `arrstore` | `CRATONVM_DBG_ARRSTORE` |
| `asserteq` | `CRATONVM_DBG_ASSERTEQ` |
| `assertj-arr` | `CRATONVM_DBG_ASSERTJ_ARR` |
| `athrow` | `CRATONVM_DBG_ATHROW` |
| `atomic-updater` | `CRATONVM_DBG_ATOMIC_UPDATER` |
| `badrecv` | `CRATONVM_DBG_BADRECV` |
| `badref` | `CRATONVM_DBG_BADREF` |
| `bb` | `CRATONVM_DBG_BB` |
| `bblp` | `CRATONVM_DBG_BBLP` |
| `bd-debug` | `CRATONVM_BD_DEBUG` |
| `blocked-access` | `CRATONVM_DBG_BLOCKED_ACCESS` |
| `blockgc` | `CRATONVM_DBG_BLOCKGC` |
| `root-remap-audit` | `CRATONVM_DBG_ROOT_REMAP_AUDIT` |
| `bufunder` | `CRATONVM_DBG_BUFUNDER` |
| `bug03` | `CRATONVM_DBG_BUG03` |
| `bytecode-dump` | `CRATONVM_DBG_BYTECODE_DUMP` |
| `callee-probe` | `CRATONVM_DBG_CALLEE_PROBE` |
| `caller` | `CRATONVM_DBG_CALLER` |
| `capval` | `CRATONVM_DBG_CAPVAL` |
| `catalina` | `CRATONVM_DBG_CATALINA` |
| `cause` | `CRATONVM_DBG_CAUSE` |
| `census-exact-invocations` | `CRATONVM_CENSUS_EXACT_INVOCATIONS` |
| `cce` | `CRATONVM_DBG_CCE` |
| `cce-bt` | `CRATONVM_DBG_CCE_BT` |
| `coll-refresh` | `CRATONVM_DBG_COLL_REFRESH` |
| `corrupt-cell` | `CRATONVM_DBG_CORRUPT_CELL` |
| `corrupt-cell-selftest` | `CRATONVM_DBG_CORRUPT_CELL_SELFTEST` |
| `ccecache` | `CRATONVM_DBG_CCECACHE` |
| `ccsprobe` | `CRATONVM_DBG_CCSPROBE` |
| `coercion` | `CRATONVM_DBG_COERCION` |
| `cellcorrupt` | `CRATONVM_DBG_CELLCORRUPT` |
| `charset` | `CRATONVM_DBG_CHARSET` |
| `class-resource` | `CRATONVM_DBG_CLASS_RESOURCE` |
| `classpath` | `CRATONVM_DBG_CLASSPATH` |
| `cleaners` | `CRATONVM_DBG_NO_CLEANERS` |
| `clinit-fail` | `CRATONVM_DBG_CLINIT_FAIL` |
| `clinit-order` | `CRATONVM_DBG_CLINIT_ORDER` |
| `clone` | `CRATONVM_DBG_CLONE` |
| `coerce` | `CRATONVM_DBG_COERCE` |
| `checkcast-inline` | `CRATONVM_DBG_CHECKCAST_INLINE` |
| `compact-inline` | `CRATONVM_DBG_COMPACT_INLINE` |
| `compact-legacy` | `CRATONVM_DBG_COMPACT_LEGACY` |
| `compactvalue` | `CRATONVM_DBG_COMPACTVALUE` |
| `component-type` | `CRATONVM_DBG_COMPONENT_TYPE` |
| `corrupt-frames` | `CRATONVM_DBG_CORRUPT_FRAMES` |
| `ctor-fix` | `CRATONVM_DBG_CTOR_FIX` |
| `dbb-elem` | `CRATONVM_DBG_DBB_ELEM` |
| `debug-sfi` | `CRATONVM_DEBUG_SFI` |
| `debug-stack-tag` | `CRATONVM_DEBUG_STACK_TAG` |
| `debug-stackwalk` | `CRATONVM_DEBUG_STACKWALK` |
| `define` | `CRATONVM_DBG_DEFINE` |
| `define-census` | `CRATONVM_DBG_DEFINE_CENSUS` |
| `deflate` | `CRATONVM_DBG_DEFLATE` |
| `deopt` | `CRATONVM_DBG_DEOPT` |
| `deopt-eager` | `CRATONVM_DEOPT_EAGER` |
| `deopt-eager-bci` | `CRATONVM_DEOPT_EAGER_BCI` |
| `deopt-verify` | `CRATONVM_DEOPT_VERIFY` |
| `deoptslot` | `CRATONVM_DBG_DEOPTSLOT` |
| `deprecations` | `CRATONVM_QUIET_DEPRECATIONS` |
| `desctrace` | `CRATONVM_DBG_DESCTRACE` |
| `diag-hib32` | `CRATONVM_DIAG_HIB32` |
| `diag-jar-list` | `CRATONVM_DIAG_JAR_LIST` |
| `diag-jboss-services` | `CRATONVM_DIAG_JBOSS_SERVICES` |
| `diag-jca` | `CRATONVM_DIAG_JCA` |
| `diag-method-invoke-null` | `CRATONVM_DIAG_METHOD_INVOKE_NULL` |
| `diag-properties` | `CRATONVM_DIAG_PROPERTIES` |
| `diag-serviceloader` | `CRATONVM_DIAG_SERVICELOADER` |
| `dispatch-tally` | `CRATONVM_DBG_DISPATCH_TALLY` |
| `dopriv` | `CRATONVM_DBG_DOPRIV` |
| `dropped-stubs` | `CRATONVM_DBG_DROPPED_STUBS` |
| `dupdef` | `CRATONVM_DBG_DUPDEF` |
| `dump-jit` | `CRATONVM_DBG_DUMP_JIT` |
| `dupcall-filter` | `CRATONVM_DBG_DUPCALL_FILTER` |
| `dupclass` | `CRATONVM_DBG_DUPCLASS` |
| `dupclass-bt` | `CRATONVM_DBG_DUPCLASS_BT` |
| `dupclass-filter` | `CRATONVM_DBG_DUPCLASS_FILTER` |
| `typecheck-filter` | `CRATONVM_DBG_TYPECHECK_FILTER` |
| `dupx-methods` | `CRATONVM_DBG_DUPX_METHODS` |
| `ecwatch` | `CRATONVM_DBG_ECWATCH` |
| `ecwatch-native` | `CRATONVM_DBG_ECWATCH_NATIVE` |
| `eintr-inject` | `CRATONVM_DBG_EINTR_INJECT` |
| `eintr-no-retry` | `CRATONVM_DBG_EINTR_NO_RETRY` |
| `enable-native-ring` | `CRATONVM_ENABLE_NATIVE_RING` |
| `eqe` | `CRATONVM_DBG_EQE` |
| `excframe` | `CRATONVM_DBG_EXCFRAME` |
| `exec` | `CRATONVM_DBG_EXEC` |
| `exec-frame-trace` | `CRATONVM_EXEC_FRAME_TRACE` |
| `exit` | `CRATONVM_DBG_EXIT` |
| `fbcglib` | `CRATONVM_DBG_FBCGLIB` |
| `fbref` | `CRATONVM_DBG_FBREF` |
| `fc-fast-io-stats` | `CRATONVM_FC_FAST_IO_STATS` |
| `field-get` | `CRATONVM_DBG_FIELD_GET` |
| `field-watch` | `CRATONVM_DBG_FIELD_WATCH` |
| `fieldaddr` | `CRATONVM_DBG_FIELDADDR` |
| `force-moving` | `CRATONVM_DBG_FORCE_MOVING` |
| `forname-trace` | `CRATONVM_FORNAME_TRACE` |
| `frame-trace` | `CRATONVM_FRAME_TRACE` |
| `fsp` | `CRATONVM_DBG_FSP` |
| `fullstack-scan` | `CRATONVM_DBG_FULLSTACK_SCAN` |
| `fwdwalk` | `CRATONVM_DBG_FWDWALK` |
| `fwdguard` | `CRATONVM_DBG_FWDGUARD` |
| `g1-dbg-headers` | `CRATONVM_G1_DBG_HEADERS` |
| `g1-dbg-pins` | `CRATONVM_G1_DBG_PINS` |
| `g1-dbg-reach` | `CRATONVM_G1_DBG_REACH` |
| `g1-dbg-rootcensus` | `CRATONVM_G1_DBG_ROOTCENSUS` |
| `gdm-prof` | `CRATONVM_DBG_GDM_PROF` |
| `g1-dbg-zero` | `CRATONVM_G1_DBG_ZERO` |
| `g1diag` | `CRATONVM_DBG_G1DIAG` |
| `g1accessor` | `CRATONVM_DBG_G1ACCESSOR` |
| `gc-array-guard-bt` | `CRATONVM_GC_ARRAY_GUARD_BT` |
| `gc-fallback-reasons` | `CRATONVM_DBG_GC_FALLBACK_REASONS` |
| `gc-overhead` | `CRATONVM_DBG_GC_OVERHEAD` |
| `gc-stats` | `CRATONVM_GC_STATS` |
| `gc-stress` | `CRATONVM_DBG_GC_STRESS` |
| `oop-oracle-force-refute` | `CRATONVM_DBG_OOP_ORACLE_FORCE_REFUTE` |
| `gc-verify-stale` | `CRATONVM_GC_VERIFY_STALE` |
| `gcpart` | `CRATONVM_DBG_GCPART` |
| `jni-localref` | `CRATONVM_DBG_JNI_LOCALREF` |
| `gcpause` | `CRATONVM_DBG_GCPAUSE` |
| `gcphase` | `CRATONVM_DBG_GCPHASE` |
| `gcwrite` | `CRATONVM_DBG_GCWRITE` |
| `getfield-receivers` | `CRATONVM_DBG_GETFIELD_RECEIVERS` |
| `getresources` | `CRATONVM_DBG_GETRESOURCES` |
| `getstatic-prof` | `CRATONVM_DBG_GETSTATIC_PROF` |
| `stack-kinds` | `CRATONVM_DBG_STACK_KINDS` |
| `stub-door` | `CRATONVM_DBG_STUB_DOOR` |
| `native-entry` | `CRATONVM_DBG_NATIVE_ENTRY` |
| `gocbf` | `CRATONVM_DBG_GOCBF` |
| `gpu-dump-ptx` | `CRATONVM_GPU_DUMP_PTX` |
| `gpu-trace-bytes` | `CRATONVM_GPU_TRACE_BYTES` |
| `gpu-time-dispatch` | `CRATONVM_GPU_TIME_DISPATCH` |
| `gse` | `CRATONVM_DBG_GSE` |
| `h2parserread` | `CRATONVM_DBG_H2PARSERREAD` |
| `h2trace` | `CRATONVM_DBG_H2TRACE` |
| `hang-sample` | `CRATONVM_DBG_HANG_SAMPLE` |
| `hangwalk` | `CRATONVM_DBG_HANGWALK` |
| `heap-stale` | `CRATONVM_DBG_HEAP_STALE` |
| `fmt-wrongtype` | `CRATONVM_DBG_FMT_WRONGTYPE` |
| `heap-trace` | `CRATONVM_DBG_HEAP_TRACE` |
| `heapcopy` | `CRATONVM_DBG_HEAPCOPY` |
| `heartbeat` | `CRATONVM_DBG_HEARTBEAT` |
| `hm-trace` | `CRATONVM_HM_TRACE` |
| `hminit-purge` | `CRATONVM_DBG_HMINIT_PURGE` |
| `hmput` | `CRATONVM_DBG_HMPUT` |
| `hotpath-counts` | `CRATONVM_DBG_HOTPATH_COUNTS` |
| `hs-itr-dbg` | `CRATONVM_HS_ITR_DBG` |
| `httpsrv` | `CRATONVM_DBG_HTTPSRV` |
| `iae-trace` | `CRATONVM_IAE_TRACE` |
| `iae-trace2` | `CRATONVM_IAE_TRACE2` |
| `imse` | `CRATONVM_DBG_IMSE` |
| `indy-all` | `CRATONVM_DBG_INDY_ALL` |
| `indy-generic` | `CRATONVM_DBG_INDY_GENERIC` |
| `inline-fr` | `CRATONVM_DBG_INLINE_FR` |
| `interrupt` | `CRATONVM_DBG_INTERRUPT` |
| `intrinsic-stats` | `CRATONVM_INTRINSIC_STATS` |
| `isolated-cnf` | `CRATONVM_DBG_ISOLATED_CNF` |
| `invoke-coerce` | `CRATONVM_DBG_INVOKE_COERCE` |
| `invoke-virtual-entry-trace` | `CRATONVM_INVOKE_VIRTUAL_ENTRY_TRACE` |
| `invokestatic-loader-trace` | `CRATONVM_INVOKESTATIC_LOADER_TRACE` |
| `invokestats` | `CRATONVM_DBG_INVOKESTATS` |
| `invspecial` | `CRATONVM_DBG_INVSPECIAL` |
| `ir-bailout` | `CRATONVM_DBG_IR_BAILOUT` |
| `ir-call` | `CRATONVM_DBG_IR_CALL` |
| `ir-bufsize` | `CRATONVM_DBG_IR_BUFSIZE` |
| `ir-compiles` | `CRATONVM_DBG_IR_COMPILES` |
| `ir-isel` | `CRATONVM_DBG_IR_ISEL` |
| `ir-linear-scan` | `CRATONVM_DBG_IR_LINEAR_SCAN` |
| `ir-long` | `CRATONVM_DBG_IR_LONG` |
| `ir-reloc` | `CRATONVM_DBG_IR_RELOC` |
| `ir-slots` | `CRATONVM_DBG_IR_SLOTS` |
| `irslot` | `CRATONVM_DBG_IRSLOT` |
| `isinstance` | `CRATONVM_DBG_ISINSTANCE` |
| `jar` | `CRATONVM_DBG_JAR` |
| `native-shadow-sink-cap` | `CRATONVM_NATIVE_SHADOW_SINK_CAP` |
| `jetty` | `CRATONVM_DBG_JETTY` |
| `jetty2` | `CRATONVM_DBG_JETTY2` |
| `jit-alloc` | `CRATONVM_DBG_JIT_ALLOC` |
| `jit-bisect-only` | `CRATONVM_JIT_BISECT_ONLY` |
| `jit-code` | `CRATONVM_DBG_JIT_CODE` |
| `jit-code-free` | `CRATONVM_DBG_JIT_CODE_FREE` |
| `jit-compiled` | `CRATONVM_DBG_JIT_COMPILED` |
| `jit-disasm` | `CRATONVM_DBG_JIT_DISASM` |
| `jit-dispatch` | `CRATONVM_DBG_JIT_DISPATCH` |
| `jit-entry` | `CRATONVM_DBG_JIT_ENTRY` |
| `jit-borrow-sites` | `CRATONVM_DBG_JIT_BORROW_SITES` |
| `jit-gen` | `CRATONVM_DBG_JIT_GEN` |
| `jit-ldc` | `CRATONVM_DBG_JIT_LDC` |
| `loop-work` | `CRATONVM_DBG_LOOP_WORK` |
| `field-site` | `CRATONVM_DBG_FIELD_SITE` |
| `field-descriptor` | `CRATONVM_DBG_FIELD_DESCRIPTOR` |
| `jit-direct-binds` | `CRATONVM_DBG_JIT_DIRECT_BINDS` |
| `jit-ea` | `CRATONVM_DBG_JIT_EA` |
| `ir-graph` | `CRATONVM_DBG_IR_GRAPH` |
| `zgc-target` | `CRATONVM_DBG_ZGC_TARGET` |
| `zgc-high` | `CRATONVM_DBG_ZGC_HIGH` |
| `jit-elide-ctor` | `CRATONVM_DBG_JIT_ELIDE_CTOR` |
| `jit-field-sites` | `CRATONVM_DBG_JIT_FIELD_SITES` |
| `g1-live-memo` | `CRATONVM_DBG_G1_LIVE_MEMO` |
| `jit-method-stats` | `CRATONVM_DBG_JIT_METHOD_STATS` |
| `jit-mic` | `CRATONVM_DBG_JIT_MIC` |
| `jit-scan-prof` | `CRATONVM_DBG_JIT_SCAN_PROF` |
| `jit-rootscan` | `CRATONVM_DBG_JIT_ROOTSCAN` |
| `remap-residue` | `CRATONVM_DBG_REMAP_RESIDUE` |
| `oopcov` | `CRATONVM_DBG_OOPCOV` |
| `xt-coverage` | `CRATONVM_DBG_XT_COVERAGE` |
| `jit-stale-after-remap` | `CRATONVM_DBG_JIT_STALE_AFTER_REMAP` |
| `jit-stale-below-rbp` | `CRATONVM_DBG_JIT_STALE_BELOW_RBP` |
| `jit-names` | `CRATONVM_DBG_JIT_NAMES` |
| `jit-pin` | `CRATONVM_DBG_JIT_PIN` |
| `jit-putfield` | `CRATONVM_DBG_JIT_PUTFIELD` |
| `jit-safepoints` | `CRATONVM_DBG_JIT_SAFEPOINTS` |
| `jit-stale-ic` | `CRATONVM_DBG_JIT_STALE_IC` |
| `jit-unmap` | `CRATONVM_DBG_JIT_UNMAP` |
| `intrinsic` | `CRATONVM_DBG_INTRINSIC` |
| `jitc` | `CRATONVM_DBG_JITC` |
| `jlm` | `CRATONVM_DBG_JLM` |
| `jul` | `CRATONVM_DBG_JUL` |
| `kcbool` | `CRATONVM_DBG_KCBOOL` |
| `lambda` | `CRATONVM_DBG_LAMBDA` |
| `lambda-dispatch` | `CRATONVM_DBG_LAMBDA_DISPATCH` |
| `lambda-generic` | `CRATONVM_DBG_LAMBDA_GENERIC` |
| `lambda-jit` | `CRATONVM_DBG_LAMBDA_JIT` |
| `lambda-prof` | `CRATONVM_DBG_LAMBDA_PROF` |
| `layout` | `CRATONVM_DBG_LAYOUT` |
| `ldc-classref-trace` | `CRATONVM_LDC_CLASSREF_TRACE` |
| `letsgo` | `CRATONVM_DBG_LETSGO` |
| `lhm-evict` | `CRATONVM_DBG_LHM_EVICT` |
| `licm` | `CRATONVM_DBG_LICM` |
| `linkage` | `CRATONVM_DBG_LINKAGE` |
| `linkage-bt` | `CRATONVM_DBG_LINKAGE_BT` |
| `linker` | `CRATONVM_DBG_LINKER` |
| `loadclass` | `CRATONVM_DBG_LOADCLASS` |
| `loader-chain` | `CRATONVM_DBG_LOADER_CHAIN` |
| `loader-trace` | `CRATONVM_DBG_LOADER_TRACE` |
| `load-transform-no-memo` | `CRATONVM_DBG_LOAD_TRANSFORM_NO_MEMO` |
| `logprov` | `CRATONVM_DBG_LOGPROV` |
| `longroot` | `CRATONVM_DBG_LONGROOT` |
| `lookup` | `CRATONVM_DBG_LOOKUP` |
| `map-miss-audit` | `CRATONVM_DBG_MAP_MISS_AUDIT` |
| `map-view-cache` | `CRATONVM_DBG_MAP_VIEW_CACHE` |
| `mapper` | `CRATONVM_DBG_MAPPER` |
| `mcl` | `CRATONVM_DBG_MCL` |
| `memwatch` | `CRATONVM_DBG_MEMWATCH` |
| `method-invoke-box` | `CRATONVM_DBG_METHOD_INVOKE_BOX` |
| `mh-adapter` | `CRATONVM_DBG_MH_ADAPTER` |
| `mh-dispatch` | `CRATONVM_DBG_MH_DISPATCH` |
| `mh-stack` | `CRATONVM_DBG_MH_STACK` |
| `mic-prof` | `CRATONVM_DBG_MIC_PROF` |
| `monitor-notify` | `CRATONVM_DBG_MONITOR_NOTIFY` |
| `mic-method` | `CRATONVM_DBG_MIC_METHOD` |
| `mark-why-class` | `CRATONVM_DBG_MARK_WHY_CLASS` |
| `mirrorpin-why` | `CRATONVM_DBG_MIRRORPIN_WHY` |
| `root-source` | `CRATONVM_DBG_ROOT_SOURCE` |
| `zgc-verify-slide` | `CRATONVM_DBG_ZGC_VERIFY_SLIDE` |
| `zgc-corpse` | `CRATONVM_DBG_ZGC_CORPSE` |
| `mapgen` | `CRATONVM_DBG_MAPGEN` |
| `vacated-frames` | `CRATONVM_DBG_VACATED_FRAMES` |
| `atomic-intrinsic` | `CRATONVM_DBG_ATOMIC_INTRINSIC` |
| `define-filter` | `CRATONVM_DBG_DEFINE_FILTER` |
| `define-stack-filter` | `CRATONVM_DBG_DEFINE_STACK_FILTER` |
| `hw-atomic` | `CRATONVM_DBG_HW_ATOMIC` |
| `jca-getinstance` | `CRATONVM_DBG_JCA_GETINSTANCE` |
| `mic-trace` | `CRATONVM_DBG_MIC_TRACE` |
| `minvoke` | `CRATONVM_DBG_MINVOKE` |
| `mirrorpin` | `CRATONVM_DBG_MIRRORPIN` |
| `modprov` | `CRATONVM_DBG_MODPROV` |
| `modstatic` | `CRATONVM_DBG_MODSTATIC` |
| `monenter` | `CRATONVM_DBG_MONENTER` |
| `monexit` | `CRATONVM_DBG_MONEXIT` |
| `moving-young-band-dbg` | `CRATONVM_MOVING_YOUNG_BAND_DBG` |
| `moving-young-coverage-dbg` | `CRATONVM_MOVING_YOUNG_COVERAGE_DBG` |
| `moving-young-fallbacks` | `CRATONVM_MOVING_YOUNG_FALLBACKS` |
| `moving-young-no-band-verify` | `CRATONVM_MOVING_YOUNG_NO_BAND_VERIFY` |
| `moving-young-verify` | `CRATONVM_MOVING_YOUNG_VERIFY` |
| `msc` | `CRATONVM_DBG_MSC` |
| `mtroots` | `CRATONVM_DBG_MTROOTS` |
| `nativelibraries-load-ok` | `CRATONVM_DBG_NATIVELIBRARIES_LOAD_OK` |
| `native-lookups` | `CRATONVM_DBG_NATIVE_LOOKUPS` |
| `ncdfe` | `CRATONVM_DBG_NCDFE` |
| `needs-exact-trace` | `CRATONVM_NEEDS_EXACT_TRACE` |
| `net` | `CRATONVM_DBG_NET` |
| `netty-queue` | `CRATONVM_DBG_NETTY_QUEUE` |
| `nextint` | `CRATONVM_DBG_NEXTINT` |
| `nio-bind` | `CRATONVM_DBG_NIO_BIND` |
| `nocode` | `CRATONVM_DBG_NOCODE` |
| `nonmoving-reclaim` | `CRATONVM_DBG_NO_NONMOVING_RECLAIM` |
| `npe-invoke` | `CRATONVM_DBG_NPE_INVOKE` |
| `npe-none` | `CRATONVM_DBG_NPE_NONE` |
| `npe-match` | `CRATONVM_DBG_NPE_MATCH` |
| `a5-engagement` | `CRATONVM_DBG_A5_ENGAGEMENT` |
| `unreg-memo-audit` | `CRATONVM_DBG_UNREG_MEMO_AUDIT` |
| `redefine-dump` | `CRATONVM_DBG_REDEFINE_DUMP` |
| `npe-stack` | `CRATONVM_DBG_NPE_STACK` |
| `npe-trace` | `CRATONVM_DBG_NPE_TRACE` |
| `nsee-trace` | `CRATONVM_NSEE_TRACE` |
| `nsme` | `CRATONVM_DBG_NSME` |
| `null-native` | `CRATONVM_DBG_NULL_NATIVE` |
| `nullthis` | `CRATONVM_DBG_NULLTHIS` |
| `obj-equals` | `CRATONVM_DBG_OBJ_EQUALS` |
| `objects` | `CRATONVM_DBG_OBJECTS` |
| `objkey` | `CRATONVM_DBG_OBJKEY` |
| `obsreg` | `CRATONVM_DBG_OBSREG` |
| `oldsweep-owners` | `CRATONVM_DBG_OLDSWEEP_OWNERS` |
| `oobfield` | `CRATONVM_DBG_OOBFIELD` |
| `oom-bt` | `CRATONVM_DBG_OOM_BT` |
| `oop-span-probe` | `CRATONVM_OOP_SPAN_PROBE` |
| `osr` | `CRATONVM_DBG_OSR` |
| `overlay-gate` | `CRATONVM_DBG_OVERLAY_GATE` |
| `owner-filter` | `CRATONVM_DBG_OWNER_FILTER` |
| `osr-exit-after` | `CRATONVM_OSR_EXIT_AFTER` |
| `osr-exit-test` | `CRATONVM_OSR_EXIT_TEST` |
| `osr-bind` | `CRATONVM_DBG_OSR_BIND` |
| `osr-frame-trace` | `CRATONVM_DBG_OSR_FRAME_TRACE` |
| `osr-meta` | `CRATONVM_DBG_OSR_META` |
| `osr-seed-collision` | `CRATONVM_DBG_OSR_SEED_COLLISION` |
| `osr-slots` | `CRATONVM_DBG_OSR_SLOTS` |
| `view-kind` | `CRATONVM_DBG_VIEWKIND` |
| `view-resync` | `CRATONVM_DBG_VIEWRESYNC` |
| `overlay` | `CRATONVM_DBG_OVERLAY` |
| `overlay-all` | `CRATONVM_DBG_OVERLAY_ALL` |
| `overlay-bt` | `CRATONVM_DBG_OVERLAY_BT` |
| `overlay-nodedup` | `CRATONVM_DBG_OVERLAY_NODEDUP` |
| `overlay-prune` | `CRATONVM_DBG_OVERLAY_PRUNE` |
| `tmview` | `CRATONVM_DBG_TMVIEW` |
| `parklat` | `CRATONVM_DBG_PARKLAT` |
| `pb` | `CRATONVM_DBG_PB` |
| `pbe` | `CRATONVM_DBG_PBE` |
| `pbstart` | `CRATONVM_DBG_PBSTART` |
| `phase-accounting` | `CRATONVM_PHASE_ACCOUNTING` |
| `phase-accounting-jfr` | `CRATONVM_PHASE_ACCOUNTING_JFR` |
| `phase-accounting-out` | `CRATONVM_PHASE_ACCOUNTING_OUT` |
| `picocli-style` | `CRATONVM_DBG_PICOCLI_STYLE` |
| `popint` | `CRATONVM_DBG_POPINT` |
| `precise` | `CRATONVM_DBG_PRECISE` |
| `promo-seed` | `CRATONVM_DBG_PROMO_SEED` |
| `proxy` | `CRATONVM_DBG_PROXY` |
| `prune` | `CRATONVM_DBG_NO_PRUNE` |
| `jit-root-scan` | `CRATONVM_DBG_NO_JIT_ROOT_SCAN` |
| `fincand` | `CRATONVM_DBG_FINCAND` |
| `punned-ref` | `CRATONVM_DBG_PUNNED_REF` |
| `view-comod` | `CRATONVM_DBG_VIEW_COMOD` |
| `quarkus-staticinit` | `CRATONVM_DBG_QUARKUS_STATICINIT` |
| `quicken-stats` | `CRATONVM_QUICKEN_STATS` |
| `quiet-env-fallback` | `CRATONVM_QUIET_ENV_FALLBACK` |
| `raf-getfd` | `CRATONVM_DBG_RAF_GETFD` |
| `raf-init` | `CRATONVM_DBG_RAF_INIT` |
| `rbc6` | `CRATONVM_DBG_RBC6` |
| `rbc6-emit` | `CRATONVM_DBG_RBC6_EMIT` |
| `re5` | `CRATONVM_DBG_RE5` |
| `refersto` | `CRATONVM_DBG_REFERSTO` |
| `reflection-factory` | `CRATONVM_DBG_REFLECTION_FACTORY` |
| `refproc` | `CRATONVM_DBG_NO_REFPROC` |
| `refproc-remark` | `CRATONVM_DBG_REFPROC_REMARK` |
| `remap-trace` | `CRATONVM_DBG_REMAP_TRACE` |
| `replovr` | `CRATONVM_DBG_REPLOVR` |
| `resolve-shim` | `CRATONVM_DBG_RESOLVE_SHIM` |
| `resource-timing` | `CRATONVM_DBG_RESOURCE_TIMING` |
| `resume-pc` | `CRATONVM_DBG_RESUME_PC` |
| `retransform` | `CRATONVM_DBG_RETRANSFORM` |
| `rootsnap` | `CRATONVM_DBG_ROOTSNAP` |
| `rootsnap-every` | `CRATONVM_DBG_ROOTSNAP_EVERY` |
| `rootsnap-verify` | `CRATONVM_DBG_ROOTSNAP_VERIFY` |
| `rootprof` | `CRATONVM_DBG_ROOTPROF` |
| `rset-audit` | `CRATONVM_DBG_RSET_AUDIT` |
| `rset-audit-young-scan` | `CRATONVM_DBG_RSET_AUDIT_YOUNG_SCAN` |
| `rterr` | `CRATONVM_DBG_RTERR` |
| `rvas` | `CRATONVM_DBG_RVAS` |
| `s111-dbg` | `CRATONVM_S111_DBG` |
| `sbload` | `CRATONVM_DBG_SBLOAD` |
| `sc-close` | `CRATONVM_DBG_SC_CLOSE` |
| `sc-read` | `CRATONVM_DBG_SC_READ` |
| `sc-write` | `CRATONVM_DBG_SC_WRITE` |
| `scalar-deopt` | `CRATONVM_DBG_SCALAR_DEOPT` |
| `scalar-new` | `CRATONVM_DBG_SCALAR_NEW` |
| `scanner-debug` | `CRATONVM_SCANNER_DEBUG` |
| `seed-all-old` | `CRATONVM_DBG_SEED_ALL_OLD` |
| `seedhunt` | `CRATONVM_DBG_SEEDHUNT` |
| `sel` | `CRATONVM_DBG_SEL` |
| `selector` | `CRATONVM_DBG_SELECTOR` |
| `setacc` | `CRATONVM_DBG_SETACC` |
| `sfi-null-trace` | `CRATONVM_SFI_NULL_TRACE` |
| `shadow` | `CRATONVM_DBG_SHADOW` |
| `shadow-depth` | `CRATONVM_DBG_SHADOW_DEPTH` |
| `shadow-reload` | `CRATONVM_DBG_SHADOW_RELOAD` |
| `shadow-sentinel` | `CRATONVM_SHADOW_SENTINEL` |
| `shadow-watch` | `CRATONVM_SHADOW_WATCH` |
| `shadow2` | `CRATONVM_DBG_SHADOW2` |
| `shadow2-filter` | `CRATONVM_DBG_SHADOW2_FILTER` |
| `sleep-trace` | `CRATONVM_DBG_SLEEP_TRACE` |
| `sock` | `CRATONVM_DBG_SOCK` |
| `sock-bytes` | `CRATONVM_DBG_SOCK_BYTES` |
| `soe` | `CRATONVM_DBG_SOE` |
| `soft-exit` | `CRATONVM_SOFT_EXIT` |
| `sp-stats` | `CRATONVM_SP_STATS` |
| `sp-trace` | `CRATONVM_SP_TRACE` |
| `sp-verify` | `CRATONVM_SP_VERIFY` |
| `spid` | `CRATONVM_DBG_SPID` |
| `spring-dbg` | `CRATONVM_SPRING_DBG` |
| `stackless` | `CRATONVM_DBG_STACKLESS` |
| `stale-objref` | `CRATONVM_DBG_STALE_OBJREF` |
| `stale-objref-cycles` | `CRATONVM_DBG_STALE_OBJREF_CYCLES` |
| `stale-recv` | `CRATONVM_DBG_STALE_RECV` |
| `stalelong` | `CRATONVM_DBG_STALELONG` |
| `stamped` | `CRATONVM_DBG_STAMPED` |
| `straystack` | `CRATONVM_DBG_STRAYSTACK` |
| `streamsupp` | `CRATONVM_DBG_STREAMSUPP` |
| `sttrace` | `CRATONVM_DBG_STTRACE` |
| `stub-bt` | `CRATONVM_DBG_STUB_BT` |
| `stubloader` | `CRATONVM_DBG_STUBLOADER` |
| `stw-census` | `CRATONVM_DBG_STW_CENSUS` |
| `stw-expected-ids` | `CRATONVM_DBG_STW_EXPECTED_IDS` |
| `stw-native-ring` | `CRATONVM_DBG_STW_NATIVE_RING` |
| `surefire-ipc-dbg` | `CRATONVM_SUREFIRE_IPC_DBG` |
| `swchain` | `CRATONVM_DBG_SWCHAIN` |
| `sweep-census` | `CRATONVM_DBG_SWEEP_CENSUS` |
| `sweep-edges` | `CRATONVM_DBG_SWEEP_EDGES` |
| `sweep-referrers` | `CRATONVM_DBG_SWEEP_REFERRERS` |
| `sweep-zero` | `CRATONVM_DBG_SWEEP_ZERO` |
| `symbolize` | `CRATONVM_SYMBOLIZE` |
| `symbolize-dbg` | `CRATONVM_SYMBOLIZE_DBG` |
| `threadreg-perf` | `CRATONVM_DBG_THREADREG_PERF` |
| `threadstart` | `CRATONVM_DBG_THREADSTART` |
| `tier-enqueue` | `CRATONVM_DBG_TIER_ENQUEUE` |
| `tlabmiss` | `CRATONVM_DBG_TLABMISS` |
| `tls-auth` | `CRATONVM_DBG_TLS_AUTH` |
| `tls-hs` | `CRATONVM_DBG_TLS_HS` |
| `tls-pls` | `CRATONVM_DBG_TLS_PLS` |
| `tls-sock` | `CRATONVM_DBG_TLS_SOCK` |
| `tls-srv` | `CRATONVM_DBG_TLS_SRV` |
| `toarray` | `CRATONVM_DBG_TOARRAY` |
| `tohex` | `CRATONVM_DBG_TOHEX` |
| `trace-arrays-hashcode` | `CRATONVM_TRACE_ARRAYS_HASHCODE` |
| `trace-classvalue` | `CRATONVM_TRACE_CLASSVALUE` |
| `trace-pti-args` | `CRATONVM_TRACE_PTI_ARGS` |
| `trace-sb-filter` | `CRATONVM_TRACE_SB_FILTER` |
| `trace-unimplemented` | `CRATONVM_TRACE_UNIMPLEMENTED` |
| `track-native` | `CRATONVM_TRACK_NATIVE` |
| `trivial-getter-verify` | `CRATONVM_TRIVIAL_GETTER_VERIFY` |
| `uclreg` | `CRATONVM_DBG_UCLREG` |
| `uclres` | `CRATONVM_DBG_UCLRES` |
| `ucltrace` | `CRATONVM_DBG_UCLTRACE` |
| `ueh-debug` | `CRATONVM_UEH_DEBUG` |
| `uncaught` | `CRATONVM_DBG_UNCAUGHT` |
| `underflow` | `CRATONVM_DBG_UNDERFLOW` |
| `unpark-miss` | `CRATONVM_DBG_UNPARK_MISS` |
| `unpin-ring` | `CRATONVM_DBG_UNPIN_RING` |
| `unroll` | `CRATONVM_DBG_UNROLL` |
| `urlcl` | `CRATONVM_DBG_URLCL` |
| `ute` | `CRATONVM_DBG_UTE` |
| `validate-new` | `CRATONVM_DBG_VALIDATE_NEW` |
| `vdisp` | `CRATONVM_DBG_VDISP` |
| `vector-intrinsics-stats` | `CRATONVM_VECTOR_INTRINSICS_STATS` |
| `verify-error` | `CRATONVM_DBG_VERIFY_ERROR` |
| `verify-inline-frame-record` | `CRATONVM_DBG_VERIFY_INLINE_FRAME_RECORD` |
| `verify-oop-maps` | `CRATONVM_DBG_VERIFY_OOP_MAPS` |
| `visitfile` | `CRATONVM_DBG_VISITFILE` |
| `vm-state` | `CRATONVM_DBG_VM_STATE` |
| `watch-cause-self` | `CRATONVM_DBG_WATCH_CAUSE_SELF` |
| `watch-cell` | `CRATONVM_DBG_WATCH_CELL` |
| `watchaddr` | `CRATONVM_DBG_WATCHADDR` |
| `watchref` | `CRATONVM_DBG_WATCHREF` |
| `weakref` | `CRATONVM_DBG_WEAKREF` |
| `wf` | `CRATONVM_DBG_WF` |
| `wf-npe` | `CRATONVM_DBG_WF_NPE` |
| `xnio-tcp` | `CRATONVM_DBG_XNIO_TCP` |
| `xt-jit-root-scan` | `CRATONVM_DBG_XT_JIT_ROOT_SCAN` |
| `young-trigger` | `CRATONVM_DBG_YOUNG_TRIGGER` |
| `youngscan` | `CRATONVM_DBG_YOUNGSCAN` |
| `youngstate` | `CRATONVM_DBG_YOUNGSTATE` |
| `zero-ranges` | `CRATONVM_DBG_ZERO_RANGES` |
| `invoke-phases` | `CRATONVM_DBG_INVOKE_PHASES` |
| `g1-dbg-rset` | `CRATONVM_G1_DBG_RSET` |

## `CRATONVM_JIT`

251 tokens.

| Token | Expands to |
| --- | --- |
| `aaload-licm` | `CRATONVM_DISABLE_AALOAD_LICM` |
| `gpu-approx-math` | `CRATONVM_GPU_APPROX_MATH` |
| `gpu-dispatch-memo` | `CRATONVM_GPU_DISPATCH_MEMO` |
| `gpu-if-convert` | `CRATONVM_GPU_IF_CONVERT` |
| `gpu-if-convert-max-ops` | `CRATONVM_GPU_IF_CONVERT_MAX_OPS` |
| `sp-ic-deny` | `CRATONVM_JIT_SP_IC_DENY` |
| `unresolved-field-substitute` | `CRATONVM_JIT_UNRESOLVED_FIELD_SUBSTITUTE` |
| `sp-ic-deopt-check` | `CRATONVM_JIT_SP_IC_DEOPT_CHECK` |
| `sp-ic-only` | `CRATONVM_JIT_SP_IC_ONLY` |
| `sp-inline-mega` | `CRATONVM_JIT_SP_INLINE_MEGA` |
| `sp-inline-mic` | `CRATONVM_JIT_SP_INLINE_MIC` |
| `sp-inline-pic` | `CRATONVM_JIT_SP_INLINE_PIC` |
| `unreg-memo-gc-reset` | `CRATONVM_JIT_UNREG_MEMO_GC_RESET` |
| `frame-bands` | `CRATONVM_JIT_NO_FRAME_BANDS` |
| `oopmap-coverage-presence-only` | `CRATONVM_JIT_OOPMAP_COVERAGE_PRESENCE_ONLY` |
| `unreg-accept-residue` | `CRATONVM_JIT_UNREG_ACCEPT_RESIDUE` |
| `activation-global-mutex` | `CRATONVM_JIT_ACTIVATION_GLOBAL_MUTEX` |
| `alloc-class-cache` | `CRATONVM_NO_JIT_ALLOC_CLASS_CACHE` |
| `alloc-spill-sink` | `CRATONVM_JIT_NO_ALLOC_SPILL_SINK` |
| `arith-licm` | `CRATONVM_DISABLE_ARITH_LICM` |
| `bce` | `CRATONVM_JIT_NO_BCE` |
| `bg-compile` | `CRATONVM_BG_COMPILE` |
| `bytecode-loop-xform` | `CRATONVM_JIT_BYTECODE_LOOP_XFORM` |
| `bulk-byte-loops` | `CRATONVM_JIT_BULK_BYTE_LOOPS` |
| `c1-vector-veto` | `CRATONVM_JIT_C1_VECTOR_VETO` |
| `census-direct-helpers` | `CRATONVM_JIT_CENSUS_DIRECT_HELPERS` |
| `nio-byte-direct-helpers` | `CRATONVM_JIT_NIO_BYTE_DIRECT_HELPERS` |
| `md-update-direct-helper` | `CRATONVM_JIT_MD_UPDATE_DIRECT_HELPER` |
| `cached-entry-owner-reuse` | `CRATONVM_JIT_CACHED_ENTRY_OWNER_REUSE` |
| `c2-first-call` | `CRATONVM_JIT_C2_FIRST_CALL` |
| `c2-supersede` | `CRATONVM_C2_SUPERSEDE` |
| `callee-oop-flush` | `CRATONVM_JIT_NO_CALLEE_OOP_FLUSH` |
| `code-cache-max-mb` | `CRATONVM_JIT_CODE_CACHE_MAX_MB` |
| `conservative-locals` | `CRATONVM_NO_CONSERVATIVE_LOCALS` |
| `ctor-direct-call` | `CRATONVM_NO_CTOR_DIRECT_CALL` |
| `osr-ctor-bind` | `CRATONVM_NO_OSR_CTOR_BIND` |
| `real-new-site-flags` | `CRATONVM_JIT_REAL_NEW_SITE_FLAGS` |
| `deny` | `CRATONVM_JIT_DENY` |
| `deopt-real` | `CRATONVM_DEOPT_REAL` |
| `direct-callee-calls` | `CRATONVM_JIT_DIRECT_CALLEE_CALLS` |
| `dispatch-cache-direct-entry` | `CRATONVM_JIT_DISPATCH_CACHE_DIRECT_ENTRY` |
| `dispatch-cache-virtual-direct-entry` | `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY` |
| `string-intrinsic-pin` | `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN` |
| `dup-x1` | `CRATONVM_JIT_NO_DUP_X1` |
| `dup-x2` | `CRATONVM_JIT_NO_DUP_X2` |
| `dup2-x2` | `CRATONVM_JIT_NO_DUP2_X2` |
| `trusted-oop-getfield` | `CRATONVM_JIT_NO_TRUSTED_OOP_GETFIELD` |
| `dupx` | `CRATONVM_JIT_NO_DUPX` |
| `dupx-eager-canon` | `CRATONVM_JIT_DUPX_EAGER_CANON` |
| `eager-callee-chain` | `CRATONVM_JIT_EAGER_CALLEE_CHAIN` |
| `enable-callee-saved-gpr-locals` | `CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS` |
| `enable-inline-new` | `CRATONVM_JIT_ENABLE_INLINE_NEW` |
| `exc-table-c2` | `CRATONVM_JIT_NO_EXC_TABLE_C2` |
| `force-c2` | `CRATONVM_JIT_FORCE_C2` |
| `native-shadow-interface-blind` | `CRATONVM_JIT_NATIVE_SHADOW_INTERFACE_BLIND` |
| `native-shadow-caller-seal` | `CRATONVM_JIT_NATIVE_SHADOW_CALLER_SEAL` |
| `gate-pass-memo` | `CRATONVM_JIT_GATE_PASS_MEMO` |
| `full-self-call-spill` | `CRATONVM_JIT_FULL_SELF_CALL_SPILL` |
| `gc-inert-selfrec` | `CRATONVM_JIT_GC_INERT_SELFREC` |
| `getfield-helper` | `CRATONVM_JIT_GETFIELD_HELPER` |
| `getstatic-helper` | `CRATONVM_JIT_GETSTATIC_HELPER` |
| `guarded-virtual-inline` | `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE` |
| `helpful-npe-opcodes` | `CRATONVM_HELPFUL_NPE_OPCODES` |
| `inclusive-bce` | `CRATONVM_JIT_INCLUSIVE_BCE` |
| `inline-allow-static` | `CRATONVM_INLINE_ALLOW_STATIC` |
| `inline-getfield` | `CRATONVM_JIT_INLINE_GETFIELD` |
| `inline-live-slot-clamp` | `CRATONVM_JIT_NO_INLINE_LIVE_SLOT_CLAMP` |
| `inline-new` | `CRATONVM_JIT_DISABLE_INLINE_NEW` |
| `inline-putfield` | `CRATONVM_NO_JIT_INLINE_PUTFIELD` |
| `inline-self-guard` | `CRATONVM_JIT_INLINE_SELF_GUARD` |
| `inline-tlab-new` | `CRATONVM_NO_JIT_INLINE_TLAB_NEW` |
| `intrinsics` | `CRATONVM_DISABLE_INTRINSICS` |
| `ir-branchy` | `CRATONVM_NO_IR_BRANCHY` |
| `ir-call` | `CRATONVM_JIT_IR_CALL` |
| `ir-call-special` | `CRATONVM_JIT_IR_CALL_SPECIAL` |
| `ir-call-virtual` | `CRATONVM_JIT_IR_CALL_VIRTUAL` |
| `ir-over-intrinsic` | `CRATONVM_JIT_IR_OVER_INTRINSIC` |
| `ir-deopt-resume` | `CRATONVM_IR_DEOPT_RESUME` |
| `ir-direct-call` | `CRATONVM_JIT_IR_DIRECT_CALL` |
| `ir-buffer-estimate` | `CRATONVM_JIT_IR_LEGACY_BUFFER_ESTIMATE` |
| `ir-fp` | `CRATONVM_JIT_IR_FP` |
| `ir-isel-shadow` | `CRATONVM_JIT_IR_ISEL_SHADOW` |
| `ir-isel-emit` | `CRATONVM_JIT_IR_ISEL_EMIT` |
| `ir-isel-verify` | `CRATONVM_JIT_IR_ISEL_VERIFY` |
| `precise-field-ops` | `CRATONVM_JIT_NO_PRECISE_FIELD_OPS` |
| `precise-getstatic-checkcast` | `CRATONVM_JIT_NO_PRECISE_GETSTATIC_CHECKCAST` |
| `precise-alloc-athrow` | `CRATONVM_JIT_NO_PRECISE_ALLOC_ATHROW` |
| `ir-linear-scan` | `CRATONVM_JIT_IR_LINEAR_SCAN` |
| `ir-long` | `CRATONVM_JIT_IR_LONG` |
| `ir-reloc-emit` | `CRATONVM_JIT_IR_RELOC_EMIT` |
| `reloc-gate-map-incomplete` | `CRATONVM_JIT_RELOC_GATE_ON_MAP_INCOMPLETE` |
| `ir-selfrec-direct` | `CRATONVM_JIT_IR_SELFREC_DIRECT` |
| `nested-trace-frames` | `CRATONVM_JIT_NO_NESTED_TRACE_FRAMES` |
| `osr-frame-dedupe` | `CRATONVM_JIT_NO_OSR_FRAME_DEDUPE` |
| `strict-install-epoch` | `CRATONVM_JIT_STRICT_INSTALL_EPOCH` |
| `kernel-reg-locals` | `CRATONVM_JIT_KERNEL_REG_LOCALS` |
| `kernel-reg-osr` | `CRATONVM_JIT_KERNEL_REG_OSR` |
| `leak-code` | `CRATONVM_JIT_LEAK_CODE` |
| `licm` | `CRATONVM_JIT_LICM` |
| `local-liveness` | `CRATONVM_NO_LOCAL_LIVENESS` |
| `local-regs` | `CRATONVM_JIT_LOCAL_REGS` |
| `long-intrinsics` | `CRATONVM_JIT_NO_LONG_INTRINSICS` |
| `long-box-direct-helpers` | `CRATONVM_JIT_LONG_BOX_DIRECT_HELPERS` |
| `varhandle-read-direct-helpers` | `CRATONVM_JIT_VARHANDLE_READ_DIRECT_HELPERS` |
| `varhandle-cas-direct-helpers` | `CRATONVM_JIT_VARHANDLE_CAS_DIRECT_HELPERS` |
| `varhandle-write-direct-helpers` | `CRATONVM_JIT_VARHANDLE_WRITE_DIRECT_HELPERS` |
| `varhandle-cas-funnel-fast` | `CRATONVM_JIT_VARHANDLE_CAS_FUNNEL_FAST` |
| `longroot-strict` | `CRATONVM_LONGROOT_STRICT` |
| `main-inline` | `CRATONVM_JIT_MAIN_INLINE` |
| `matrix-dot` | `CRATONVM_JIT_MATRIX_DOT` |
| `metrics` | `CRATONVM_JIT_METRICS` |
| `metrics-out` | `CRATONVM_JIT_METRICS_OUT` |
| `metrics-ring` | `CRATONVM_JIT_METRICS_RING` |
| `mic-exc-table-publish` | `CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH` |
| `direct-exc-table-publish` | `CRATONVM_JIT_DIRECT_EXC_TABLE_PUBLISH` |
| `new-class-init-memo` | `CRATONVM_JIT_NO_NEW_CLASS_INIT_MEMO` |
| `mic-rust-entry-cache` | `CRATONVM_JIT_NO_MIC_RUST_ENTRY_CACHE` |
| `my-scratch-flush` | `CRATONVM_JIT_MY_SCRATCH_FLUSH` |
| `my-selfcall-proof` | `CRATONVM_JIT_MY_SELFCALL_PROOF` |
| `my-shadow-emission` | `CRATONVM_JIT_MY_SHADOW_EMISSION` |
| `native-ec-multiply` | `CRATONVM_NATIVE_EC_MULTIPLY` |
| `native-matcher-find` | `CRATONVM_NATIVE_MATCHER_FIND` |
| `native-pbe-keyfactory` | `CRATONVM_NATIVE_PBE_KEYFACTORY` |
| `native-string-regex` | `CRATONVM_NATIVE_STRING_REGEX` |
| `never-free-code` | `CRATONVM_JIT_NEVER_FREE_CODE` |
| `old-sweep-jit` | `CRATONVM_OLD_SWEEP_JIT` |
| `osr` | `CRATONVM_JIT_OSR` |
| `osr-athrow` | `CRATONVM_JIT_OSR_ATHROW` |
| `osr-dead-locals` | `CRATONVM_JIT_OSR_DEAD_LOCALS` |
| `osr-ambiguous-dead` | `CRATONVM_JIT_NO_OSR_AMBIGUOUS_DEAD` |
| `osr-refined-ref` | `CRATONVM_JIT_NO_OSR_REFINED_REF` |
| `osr-dead-mask-blanket` | `CRATONVM_JIT_OSR_DEAD_MASK_BLANKET` |
| `osr-newarray` | `CRATONVM_OSR_NEWARRAY` |
| `osr-exc-table` | `CRATONVM_JIT_OSR_EXC_TABLE` |
| `staged-arg-shadow` | `CRATONVM_JIT_NO_STAGED_ARG_SHADOW` |
| `ic-frame-republish` | `CRATONVM_JIT_NO_IC_FRAME_REPUBLISH` |
| `checkcast-inline` | `CRATONVM_JIT_CHECKCAST_INLINE` |
| `final-devirt` | `CRATONVM_JIT_FINAL_DEVIRT` |
| `inline-calls` | `CRATONVM_JIT_INLINE_CALLS` |
| `inline-nest` | `CRATONVM_JIT_INLINE_NEST` |
| `inline-call-dispatch` | `CRATONVM_JIT_INLINE_CALL_DISPATCH` |
| `inline-splice-devirt` | `CRATONVM_JIT_INLINE_SPLICE_DEVIRT` |
| `local-handlers` | `CRATONVM_JIT_LOCAL_HANDLERS` |
| `osr-seed-frame-slots` | `CRATONVM_JIT_OSR_SEED_FRAME_SLOTS` |
| `osr-strip-all-high-halves` | `CRATONVM_JIT_OSR_STRIP_ALL_HIGH_HALVES` |
| `osr-single-pc` | `CRATONVM_JIT_OSR_SINGLE_PC` |
| `poison-free` | `CRATONVM_JIT_POISON_FREE` |
| `precise-coverage-pin` | `CRATONVM_PRECISE_COVERAGE_PIN` |
| `callee-handler-precise-frame` | `CRATONVM_NO_JIT_CALLEE_HANDLER_PRECISE_FRAME` |
| `precise-handler-frames` | `CRATONVM_NO_JIT_PRECISE_HANDLER_FRAMES` |
| `precise-inline-frame-record` | `CRATONVM_NO_PRECISE_INLINE_FRAME_RECORD` |
| `precise-jit-maps` | `CRATONVM_NO_PRECISE_JIT_MAPS` |
| `precise-reg-spill` | `CRATONVM_NO_PRECISE_REG_SPILL` |
| `precise-virtual-invokes` | `CRATONVM_JIT_NO_PRECISE_VIRTUAL_INVOKES` |
| `range-bce` | `CRATONVM_JIT_RANGE_BCE` |
| `range-scan-legacy` | `CRATONVM_JIT_RANGE_SCAN_LEGACY` |
| `reassoc` | `CRATONVM_JIT_REASSOC` |
| `retpc-validate` | `CRATONVM_JIT_NO_RETPC_VALIDATE` |
| `native-site-cache` | `CRATONVM_JIT_NO_NATIVE_SITE_CACHE` |
| `rootsnap-cache` | `CRATONVM_ROOTSNAP_CACHE` |
| `rootsnap-cache-survive-gc` | `CRATONVM_ROOTSNAP_CACHE_SURVIVE_GC` |
| `safepoint-polls` | `CRATONVM_JIT_SAFEPOINT_POLLS` |
| `safepoint-reg-spill` | `CRATONVM_JIT_SAFEPOINT_REG_SPILL` |
| `scalar-deopt` | `CRATONVM_SCALAR_DEOPT` |
| `scalar-new` | `CRATONVM_JIT_SCALAR_NEW` |
| `scalar-replacement` | `CRATONVM_DISABLE_SCALAR_REPLACEMENT` |
| `scan-cache` | `CRATONVM_NO_JIT_SCAN_CACHE` |
| `self-cache-inherit` | `CRATONVM_JIT_NO_SELF_CACHE_INHERIT` |
| `atomic-intrinsic` | `CRATONVM_JIT_NO_ATOMIC_INTRINSIC` |
| `ffm-intrinsic` | `CRATONVM_JIT_NO_FFM_INTRINSIC` |
| `c2-alloc-upgrade` | `CRATONVM_JIT_C2_ALLOC_UPGRADE` |
| `ir-inline` | `CRATONVM_JIT_IR_INLINE` |
| `field-site-cache` | `CRATONVM_JIT_FIELD_SITE_CACHE` |
| `cast-site-cache` | `CRATONVM_JIT_NO_CAST_SITE_CACHE` |
| `code-ptr-memo` | `CRATONVM_JIT_NO_CODE_PTR_MEMO` |
| `param-tag-scan` | `CRATONVM_JIT_NO_PARAM_TAG_SCAN` |
| `ldc-const-cache` | `CRATONVM_JIT_NO_LDC_CONST_CACHE` |
| `ir-unresumable-trap-guard` | `CRATONVM_JIT_IR_UNRESUMABLE_TRAP_GUARD` |
| `compiled-ldc-const-cache` | `CRATONVM_JIT_COMPILED_LDC_CONST_CACHE` |
| `new-site-cache` | `CRATONVM_JIT_NO_NEW_SITE_CACHE` |
| `site-cache` | `CRATONVM_JIT_SITE_CACHE` |
| `unreg-memo-hiwater` | `CRATONVM_JIT_UNREG_MEMO_HIWATER` |
| `field-site-cache-loader` | `CRATONVM_JIT_FIELD_SITE_CACHE_LOADER` |
| `field-site-slots` | `CRATONVM_JIT_FIELD_SITE_SLOTS` |
| `method-site-cache` | `CRATONVM_JIT_METHOD_SITE_CACHE` |
| `loop-work-tierup` | `CRATONVM_JIT_LOOP_WORK_TIERUP` |
| `shadow-nopush` | `CRATONVM_SHADOW_NOPUSH` |
| `sync-methods` | `CRATONVM_JIT_SYNC_METHODS` |
| `shadow-noreload` | `CRATONVM_SHADOW_NORELOAD` |
| `shadow-pin` | `CRATONVM_SHADOW_PIN` |
| `shadow-raw-reload` | `CRATONVM_SHADOW_RAW_RELOAD` |
| `shadow-end-guard` | `CRATONVM_SHADOW_NO_END_GUARD` |
| `shadow-overflow-diag` | `CRATONVM_SHADOW_OVERFLOW_DIAG` |
| `shadow-savebase` | `CRATONVM_SHADOW_NO_SAVEBASE` |
| `shadow-stack` | `CRATONVM_SHADOW_STACK` |
| `slot-mirror` | `CRATONVM_JIT_NO_SLOT_MIRROR` |
| `sp-coalesce` | `CRATONVM_SP_NO_COALESCE` |
| `sp-inline-ic` | `CRATONVM_JIT_SP_INLINE_IC` |
| `self-tailcall` | `CRATONVM_JIT_SELF_TAILCALL` |
| `sp-tailcall` | `CRATONVM_JIT_SP_TAILCALL` |
| `spec-bce` | `CRATONVM_JIT_NO_SPEC_BCE` |
| `stack-bang` | `CRATONVM_JIT_STACK_BANG / CRATONVM_JIT_NO_STACK_BANG` |
| `static-bytecode-callee` | `CRATONVM_JIT_STATIC_BYTECODE_CALLEE` |
| `indy-bridge` | `CRATONVM_JIT_INDY_BRIDGE` |
| `virtual-bytecode-callee` | `CRATONVM_JIT_VIRTUAL_BYTECODE_CALLEE` |
| `statics-index` | `CRATONVM_NO_STATICS_INDEX` |
| `vector-intrinsics` | `CRATONVM_VECTOR_INTRINSICS` |
| `vector-templates` | `CRATONVM_VECTOR_TEMPLATES` |
| `fc-fast-io` | `CRATONVM_FC_FAST_IO` |
| `strict-callee-roots` | `CRATONVM_JIT_STRICT_CALLEE_ROOTS` |
| `strict-jit-roots` | `CRATONVM_STRICT_JIT_ROOTS` |
| `threshold` | `CRATONVM_JIT_THRESHOLD` |
| `charseq-string-intrinsic` | `CRATONVM_JIT_CHARSEQ_STRING_INTRINSIC` |
| `receiver-despec` | `CRATONVM_JIT_RECEIVER_DESPEC` |
| `despec-spare-factor` | `CRATONVM_JIT_DESPEC_SPARE_FACTOR` |
| `call-spill-elision` | `CRATONVM_JIT_CALL_SPILL_ELISION` |
| `spill-narrow` | `CRATONVM_JIT_SPILL_NARROW` |
| `spill-args-published` | `CRATONVM_JIT_SPILL_ARGS_PUBLISHED` |
| `callee-identity` | `CRATONVM_JIT_CALLEE_IDENTITY` |
| `elide-trivial-ctor` | `CRATONVM_JIT_ELIDE_TRIVIAL_CTOR` |
| `site-cache-stubs` | `CRATONVM_JIT_SITE_CACHE_STUBS` |
| `atomic-long-intrinsic` | `CRATONVM_JIT_NO_ATOMIC_LONG_INTRINSIC` |
| `tier-c1-threshold` | `CRATONVM_TIER_C1_THRESHOLD` |
| `tier-c2-min-invocations` | `CRATONVM_TIER_C2_MIN_INVOCATIONS` |
| `tier-c2-threshold` | `CRATONVM_TIER_C2_THRESHOLD` |
| `tier-osr-backedge` | `CRATONVM_TIER_OSR_BACKEDGE` |
| `tier-osr-threshold` | `CRATONVM_TIER_OSR_THRESHOLD` |
| `tier-pgo` | `CRATONVM_TIER_PGO` |
| `tiered` | `CRATONVM_TIER_ENABLED` |
| `tlab-zero-elision` | `CRATONVM_NO_JIT_TLAB_ZERO_ELISION` |
| `trivial-getter` | `CRATONVM_TRIVIAL_GETTER` |
| `unroll` | `CRATONVM_JIT_UNROLL / CRATONVM_DISABLE_UNROLL` |
| `vectorize` | `CRATONVM_JIT_VECTORIZE` |
| `verify-arena-order` | `CRATONVM_JIT_VERIFY_ARENA_ORDER` |
| `verify-frame-states` | `CRATONVM_JIT_VERIFY_FRAME_STATES` |
| `verify-ir` | `CRATONVM_JIT_VERIFY_IR` |
| `verify-memory-chain` | `CRATONVM_JIT_VERIFY_MEMORY_CHAIN` |
| `lambda-adapter` | `CRATONVM_JIT_LAMBDA_ADAPTER` |
| `lambda-capture-adapter` | `CRATONVM_JIT_LAMBDA_CAPTURE_ADAPTER` |
| `lambda-const-probe` | `CRATONVM_JIT_LAMBDA_CONST_PROBE` |
| `fjp-subclass-blocklist` | `CRATONVM_JIT_FJP_SUBCLASS_BLOCKLIST` |
| `lambda-site` | `CRATONVM_JIT_LAMBDA_SITE` |
| `lambda-tierup` | `CRATONVM_JIT_LAMBDA_TIERUP` |
| `verify-schedule` | `CRATONVM_JIT_VERIFY_SCHEDULE` |
| `verify-types` | `CRATONVM_JIT_VERIFY_TYPES` |
| `virtual-tierup` | `CRATONVM_JIT_VIRTUAL_TIERUP` |
| `xt-helper-window-scan` | `CRATONVM_XT_HELPER_WINDOW_SCAN` |
| `xt-jit-root-scan` | `CRATONVM_XT_JIT_ROOT_SCAN` |
| `xt-peer-deadline-ms` | `CRATONVM_XT_PEER_DEADLINE_MS` |
| `xt-peer-total-ms` | `CRATONVM_XT_PEER_TOTAL_MS` |
| `zero-spid` | `CRATONVM_JIT_ZERO_SPID` |

## `CRATONVM_GC`

83 tokens.

| Token | Expands to |
| --- | --- |
| `forced-finalizers` | `CRATONVM_FORCED_FINALIZERS` |
| `moving-young-bounds-guard` | `CRATONVM_MOVING_YOUNG_NO_BOUNDS_GUARD` |
| `card-metrics` | `CRATONVM_GC_CARD_METRICS` |
| `card-table-only` | `CRATONVM_CARD_TABLE_ONLY` |
| `compact-ref-fields` | `CRATONVM_COMPACT_REF_FIELDS` |
| `pack-fields-by-width` | `CRATONVM_PACK_FIELDS_BY_WIDTH` |
| `compressed-oops` | `CRATONVM_COMPRESSED_OOPS` |
| `default-heap-ergonomics` | `CRATONVM_DEFAULT_HEAP_ERGONOMICS` |
| `default-heap-max-mb` | `CRATONVM_DEFAULT_HEAP_MAX_MB` |
| `defrag-promote` | `CRATONVM_NO_DEFRAG_PROMOTE` |
| `exact-refproc-survival` | `CRATONVM_NO_EXACT_REFPROC_SURVIVAL` |
| `referent-identity-screen` | `CRATONVM_NO_REFERENT_IDENTITY_SCREEN` |
| `g1-coverage-pin` | `CRATONVM_G1_COVERAGE_PIN` |
| `g1-pin-empty-publication` | `CRATONVM_G1_PIN_EMPTY_PUBLICATION` |
| `g1-precise-only-roots` | `CRATONVM_G1_PRECISE_ONLY_ROOTS` |
| `precise-only-roots` | `CRATONVM_GC_PRECISE_ONLY_ROOTS` |
| `g1-evac-retry` | `CRATONVM_G1_NO_EVAC_RETRY` |
| `g1-live-region-memo` | `CRATONVM_G1_NO_LIVE_REGION_MEMO` |
| `g1-parallel-evac` | `CRATONVM_G1_PARALLEL_EVAC` |
| `g1-eager-humongous` | `CRATONVM_G1_EAGER_HUMONGOUS` |
| `g1-young-pause-target` | `CRATONVM_G1_YOUNG_PAUSE_TARGET` |
| `g1-scrub-free` | `CRATONVM_G1_SCRUB_FREE` |
| `g1-narrow-fixup` | `CRATONVM_G1_NARROW_FIXUP` |
| `g1-workers` | `CRATONVM_G1_WORKERS` |
| `g1-rset-source-cap` | `CRATONVM_G1_RSET_SOURCE_CAP` |
| `g1-verify-budget` | `CRATONVM_G1_VERIFY_BUDGET` |
| `gpu-chunk-streams` | `CRATONVM_GPU_CHUNK_STREAMS` |
| `gpu-chunks` | `CRATONVM_GPU_CHUNKS` |
| `gpu-critical-lease-ms` | `CRATONVM_GPU_CRITICAL_LEASE_MS` |
| `gpu-critical-wait-ms` | `CRATONVM_GPU_CRITICAL_WAIT_MS` |
| `gpu-zerocopy` | `CRATONVM_GPU_NO_ZEROCOPY` |
| `lhm-root-all` | `CRATONVM_LHM_ROOT_ALL` |
| `max-inflated-bytes` | `CRATONVM_MAX_INFLATED_BYTES` |
| `mirror-pin-young-defer` | `CRATONVM_NO_MIRROR_PIN_YOUNG_DEFER` |
| `moving-young` | `CRATONVM_MOVING_YOUNG / CRATONVM_NO_MOVING_YOUNG` |
| `moving-young-jit-frames` | `CRATONVM_MOVING_YOUNG_NO_JIT` |
| `format-arg-pin` | `CRATONVM_NO_FORMAT_ARG_PIN` |
| `targeted-compaction` | `CRATONVM_ZGC_TARGETED_COMPACTION` |
| `band-map-liveness` | `CRATONVM_GC_NO_BAND_MAP_LIVENESS` |
| `cm-id-pairing` | `CRATONVM_GC_NO_CM_ID_PAIRING` |
| `register-image-remap` | `CRATONVM_REGISTER_IMAGE_REMAP` |
| `innermost-callee-resolve` | `CRATONVM_GC_NO_CALLEE_RESOLVE` |
| `old-interior-pins` | `CRATONVM_GC_NO_OLD_INTERIOR_PINS` |
| `empty-object-run` | `CRATONVM_GC_NO_EMPTY_OBJECT_RUN` |
| `oldgen-coalesce` | `CRATONVM_NO_OLDGEN_COALESCE` |
| `oldgen-compact` | `CRATONVM_OLDGEN_COMPACT` |
| `overhead-limit` | `CRATONVM_GC_OVERHEAD_LIMIT` |
| `owner-class-filter` | `CRATONVM_OWNER_CLASS_FILTER` |
| `par-min-bytes` | `CRATONVM_GC_PAR_MIN_BYTES` |
| `par-threads` | `CRATONVM_GC_PAR_THREADS` |
| `promotion-guard` | `CRATONVM_NO_GC_PROMOTION_GUARD` |
| `promotion-oom-guard-broad` | `CRATONVM_PROMOTION_OOM_GUARD_BROAD` |
| `selective-promote` | `CRATONVM_NO_SELECTIVE_PROMOTE` |
| `stress` | `CRATONVM_GC_STRESS` |
| `sweep-anchor-stride` | `CRATONVM_GC_SWEEP_ANCHOR_STRIDE` |
| `tlab-gc-trigger` | `CRATONVM_TLAB_GC_TRIGGER` |
| `weakref-clear` | `CRATONVM_WEAKREF_CLEAR` |
| `youngscan-stride` | `CRATONVM_YOUNGSCAN_STRIDE` |
| `zgc-parmark` | `CRATONVM_ZGC_PARMARK` |
| `zgc-relocate` | `CRATONVM_ZGC_RELOCATE` |
| `zgc-relocate-proven-jit` | `CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT` |
| `zgc-high-compaction` | `CRATONVM_ZGC_HIGH_COMPACTION` |
| `zgc-tlab-starved-recycle` | `CRATONVM_ZGC_TLAB_STARVED_RECYCLE` |
| `zgc-publish-vacated` | `CRATONVM_ZGC_PUBLISH_VACATED` |
| `xt-jit-coverage-handshake` | `CRATONVM_XT_JIT_COVERAGE_HANDSHAKE` |
| `osr-coverage-shadow` | `CRATONVM_OSR_COVERAGE_SHADOW` |
| `zgc-jit-read-bounds` | `CRATONVM_ZGC_NO_JIT_READ_BOUNDS` |
| `zgc-conc-start` | `CRATONVM_ZGC_CONC_START` |
| `zgc-conc-workers` | `CRATONVM_ZGC_CONC_WORKERS` |
| `zgc-generational` | `CRATONVM_ZGC_GENERATIONAL` |
| `zgc-gen-promotion-age` | `CRATONVM_ZGC_GEN_PROMOTION_AGE` |
| `zgc-gen-minors-per-major` | `CRATONVM_ZGC_GEN_MINORS_PER_MAJOR` |
| `zgc-gen-nursery-percent` | `CRATONVM_ZGC_GEN_NURSERY_PERCENT` |
| `zgc-sweep-header-zero` | `CRATONVM_ZGC_SWEEP_HEADER_ZERO` |
| `zgc-sweep-dead-runs` | `CRATONVM_ZGC_SWEEP_DEAD_RUNS` |
| `zgc-mark-ctx-direct` | `CRATONVM_ZGC_MARK_CTX_DIRECT` |
| `zgc-startbits` | `CRATONVM_ZGC_STARTBITS` |
| `zgc-tlab` | `CRATONVM_ZGC_TLAB` |
| `young-pause-goal-ms` | `CRATONVM_GC_YOUNG_PAUSE_MS` |
| `validate-once` | `CRATONVM_GC_NO_VALIDATE_ONCE` |
| `stream-refresh-each` | `CRATONVM_GC_STREAM_REFRESH_EACH` |
| `noflag-deposit-skip-jit-scan` | `CRATONVM_GC_NOFLAG_DEPOSIT_SKIP_JIT_SCAN` |
| `identity-hash-evict` | `CRATONVM_IDENTITY_HASH_EVICT` |

## `CRATONVM_REAL`

29 tokens.

| Token | Expands to |
| --- | --- |
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

14 tokens.

| Token | Expands to |
| --- | --- |
| `allow-jsr-ret` | `CRATONVM_ALLOW_JSR_RET` |
| `enforce-native-shadow` | `CRATONVM_ENFORCE_NATIVE_SHADOW` |
| `cf-delegating-yield` | `CRATONVM_CF_DELEGATING_YIELD` |
| `aware-resolution` | `CRATONVM_LOADER_AWARE_RESOLUTION` |
| `boot-module-registry` | `CRATONVM_BOOT_MODULE_REGISTRY` |
| `cl-bootstrap-scoped` | `CRATONVM_CL_BOOTSTRAP_SCOPED` |
| `fwd-resolve-strict` | `CRATONVM_FWD_RESOLVE_STRICT` |
| `jar-mmap` | `CRATONVM_DISABLE_JAR_MMAP` |
| `lenient-clinit` | `CRATONVM_LENIENT_CLINIT` |
| `longrewrite-loose` | `CRATONVM_LONGREWRITE_LOOSE` |
| `parent-chain` | `CRATONVM_LOADER_PARENT_CHAIN` |
| `resolve-cache-cap` | `CRATONVM_RESOLVE_CACHE_CAP` |
| `stub-delegation` | `CRATONVM_CL_STUB_DELEGATION` |
| `unload` | `CRATONVM_LOADER_UNLOAD` |

## `CRATONVM_IO`

9 tokens.

| Token | Expands to |
| --- | --- |
| `canon-openfile` | `CRATONVM_CANON_OPENFILE` |
| `http-max-body` | `CRATONVM_HTTP_MAX_BODY` |
| `netty-queue-bridge` | `CRATONVM_NETTY_QUEUE_BRIDGE` |
| `resolve-outbound-host` | `CRATONVM_RESOLVE_OUTBOUND_HOST` |
| `select-max-block-ms` | `CRATONVM_SELECT_MAX_BLOCK_MS` |
| `selector-connect-probe` | `CRATONVM_NO_SELECTOR_CONNECT_PROBE` |
| `socket-capture` | `CRATONVM_SOCKET_CAPTURE` |
| `uri-strict-chars` | `CRATONVM_URI_STRICT_CHARS` |
| `zip-max-entry-bytes` | `CRATONVM_ZIP_MAX_ENTRY_BYTES` |

## `CRATONVM_THREADS`

22 tokens.

| Token | Expands to |
| --- | --- |
| `jmx-owned-synchronizers` | `CRATONVM_JMX_OWNED_SYNCHRONIZERS` |
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
| `win-hires-park` | `CRATONVM_WIN_HIRES_PARK` |

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

18 tokens.

| Token | Expands to |
| --- | --- |
| `field-resolution-name-only` | `CRATONVM_FIELD_RESOLUTION_NAME_ONLY` |
| `map-iterator-failfast` | `CRATONVM_NO_MAP_ITERATOR_FAILFAST` |
| `map-view-cache` | `CRATONVM_MAP_VIEW_CACHE` |
| `verify-map-view-cache` | `CRATONVM_VERIFY_MAP_VIEW_CACHE` |
| `eager-streams` | `CRATONVM_EAGER_STREAMS` |
| `foreign-attach` | `CRATONVM_FOREIGN_ATTACH` |
| `jboss-boot-log-file` | `CRATONVM_JBOSS_BOOT_LOG_FILE` |
| `jboss-brute-force-jars` | `CRATONVM_JBOSS_BRUTE_FORCE_JARS` |
| `jboss-logger-base-emit` | `CRATONVM_JBOSS_LOGGER_BASE_EMIT` |
| `jboss-logger-level-filter` | `CRATONVM_JBOSS_LOGGER_LEVEL_FILTER` |
| `jboss-mp-root` | `CRATONVM_JBOSS_MP_ROOT` |
| `lazy-streams` | `CRATONVM_LAZY_STREAMS` |
| `mh-strict-invokeexact` | `CRATONVM_MH_STRICT_INVOKEEXACT` |
| `mockito-legacy-selectors` | `CRATONVM_MOCKITO_LEGACY_SELECTORS` |
| `stackwalker-jdk-walk` | `CRATONVM_SW_JDK_WALK` |
| `strict-swallows` | `CRATONVM_STRICT_SWALLOWS` |
| `tomcat-mapper-natives` | `CRATONVM_TOMCAT_MAPPER_NATIVES` |
| `vh-strict-reference-return` | `CRATONVM_VH_STRICT_REFERENCE_RETURN` |

## `CRATONVM_TEST`

9 tokens.

| Token | Expands to |
| --- | --- |
| `force-win-build` | `CRATONVM_FORCE_WIN_BUILD` |
| `jdk` | `CRATONVM_TEST_JDK` |
| `segv` | `CRATONVM_TEST_SEGV` |
| `soak-iters` | `CRATONVM_SOAK_ITERS` |
| `soak-k` | `CRATONVM_SOAK_K` |
| `soak-method` | `CRATONVM_SOAK_METHOD` |
| `soak-timeout-secs` | `CRATONVM_SOAK_TIMEOUT_SECS` |
| `soak-xmx` | `CRATONVM_SOAK_XMX` |
| `var` | `CRATONVM_TEST_VAR` |

