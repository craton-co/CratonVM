# CratonVM `CRATONVM_*` environment-flag census

*Generated 2026-07-25 from `origin/dev` @ `197ed836b` by a mechanical scan of every
`.rs` file in the workspace plus every `.md` / `.sh` / `.java` / `.toml` under the
repo root. Regenerate with the scripts recorded at the bottom of this file.*

This census is the evidence base for the typed-config refactor
(`refactor/typed-vmconfig-20260725`). It exists on its own merit: it is the first
complete inventory of the flag surface, and it is what makes the migration
reviewable — a flag that silently stops being read is a silent behaviour change,
and the only defence is knowing what the full set was beforehand.

## 1. Totals

| Metric | Count |
| --- | ---: |
| Distinct `CRATONVM_*` identifiers seen anywhere (code, docs, scripts) | **689** |
| …of which have at least one Rust read site | **556** |
| Rust code literal sites (all kinds) | **1226** |
| Rust *read* sites (excludes `set_var`/`env_remove`/`option_env!`) | **1145** |
| Read sites **outside** `native-builtins/` (this refactor's scope) | **779** |
| Read sites inside `native-builtins/` (deliberately deferred, see §6) | **366** |
| Read sites that are **not** `OnceLock`-cached | **911** |
| In-process `set_var` / `remove_var` / `Command::env` sites | **72** |

### Classification

| Class | Meaning | Count |
| --- | --- | ---: |
| **(a) debug / diagnostic** | only gates `eprintln!`/tracing/extra verification; removing it cannot change a program's result | 340 |
| **(b) semantics-changing** | selects a different code path, algorithm, layout or default; two settings are two different VMs | 198 |
| **(c) test-only** | read only from `tests/`, `benches/`, `build.rs` or a soak/difftest harness | 18 |
| **(d) dead** | **no Rust read site at all** — referenced only by docs, scripts or comments | 133 |
| | | **689** |

The (b) count is the headline number. 2^198 is not a testable behaviour space, and
the census confirms the premise: the flags have become the de-facto bug-triage
mechanism, with ~1 flag added per fixed bug and no retirement path.

## 2. Class (d) — dead flags

These identifiers have **zero** Rust read sites. Every one of them is referenced
only by prose, a runbook, or a shell script. Nothing sets a value that any code
will ever observe. Grouped by why they are dead:

### Per-application `*_REAL` switches driven by `scripts/real-run-all.sh` (34)

| Flag | Referenced from |
| --- | --- |
| `CRATONVM_ACTIVEMQ_REAL` | scripts:1 |
| `CRATONVM_BYTEBUDDY_REAL` | scripts:1 |
| `CRATONVM_CASSANDRA_REAL` | scripts:1 |
| `CRATONVM_CAS_REAL` | scripts:1 |
| `CRATONVM_CGLIB_REAL` | scripts:1 |
| `CRATONVM_ECLIPSE_REAL` | docs:2,scripts:1 |
| `CRATONVM_ES_REAL` | scripts:1 |
| `CRATONVM_FELIX_REAL` | scripts:1 |
| `CRATONVM_FLINK_REAL` | scripts:1 |
| `CRATONVM_FREEMIND_REAL` | scripts:1 |
| `CRATONVM_GRADLE_REAL` | scripts:1 |
| `CRATONVM_GRPC_REAL` | scripts:1 |
| `CRATONVM_HADOOP_REAL` | scripts:1 |
| `CRATONVM_HAZELCAST_REAL` | scripts:1 |
| `CRATONVM_HBASE_REAL` | scripts:1 |
| `CRATONVM_IGNITE_REAL` | scripts:1 |
| `CRATONVM_JDOWNLOADER_REAL` | scripts:1 |
| `CRATONVM_JEDIT_REAL` | scripts:1 |
| `CRATONVM_JENKINS_REAL` | scripts:1 |
| `CRATONVM_JETTY_REAL` | scripts:1 |
| `CRATONVM_KAFKA_REAL` | scripts:1 |
| `CRATONVM_KC16_REAL` | scripts:1 |
| `CRATONVM_KC26_REAL` | scripts:1 |
| `CRATONVM_LIBERTY_REAL` | scripts:1 |
| `CRATONVM_MINDUSTRY_REAL` | scripts:1 |
| `CRATONVM_NEO4J_REAL` | scripts:1 |
| `CRATONVM_NETBEANS_REAL` | scripts:1 |
| `CRATONVM_NEXUS_REAL` | scripts:1 |
| `CRATONVM_PAYARA_REAL` | scripts:1 |
| `CRATONVM_RABBITMQ_REAL` | scripts:1 |
| `CRATONVM_SOLR_REAL` | scripts:1 |
| `CRATONVM_SONAR_REAL` | scripts:1 |
| `CRATONVM_SPARK_REAL` | scripts:1 |
| `CRATONVM_WILDFLY_REAL` | scripts:1 |

### Per-application `*_EXE` launcher overrides referenced only by `apps/` (4)

| Flag | Referenced from |
| --- | --- |
| `CRATONVM_ELASTICSEARCH_EXE` | apps:5 |
| `CRATONVM_EXE` | apps:6,docs:6 |
| `CRATONVM_KEYCLOAK_EXE` | apps:5 |
| `CRATONVM_SPRING_BOOT_EXE` | apps:5 |

### `CRATONVM_DBG_*` debug flags whose code was deleted with the bug (45)

| Flag | Referenced from |
| --- | --- |
| `CRATONVM_DBG_A5` | docs:1 |
| `CRATONVM_DBG_ARENA` | docs:1 |
| `CRATONVM_DBG_BUG24` | docs:1 |
| `CRATONVM_DBG_BUGB` | docs:1 |
| `CRATONVM_DBG_CCE_TRACE` | docs:1 |
| `CRATONVM_DBG_CDL` | docs:1 |
| `CRATONVM_DBG_CL_SCOPE` | docs:1 |
| `CRATONVM_DBG_DOPRIV_NULL` | docs:1 |
| `CRATONVM_DBG_EC` | docs:1 |
| `CRATONVM_DBG_EXC_HANDLER_MATCH` | docs:1 |
| `CRATONVM_DBG_EXEC_INTERFACE` | docs:1 |
| `CRATONVM_DBG_FCBN` | docs:1 |
| `CRATONVM_DBG_FDRACE` | docs:1 |
| `CRATONVM_DBG_FIELD_INTROSPECT` | docs:1 |
| `CRATONVM_DBG_FORCE_NONMOVING` | docs:2 |
| `CRATONVM_DBG_GPU_VERBOSE` | docs:1 |
| `CRATONVM_DBG_HCMH0706` | docs:2 |
| `CRATONVM_DBG_HDR_BT` | docs:2 |
| `CRATONVM_DBG_ISRTRACE` | docs:2 |
| `CRATONVM_DBG_JITBAIL` | docs:1 |
| `CRATONVM_DBG_JIT_COMPILE` | docs:1 |
| `CRATONVM_DBG_JIT_PF11` | docs:1 |
| `CRATONVM_DBG_MONIMSE` | docs:1 |
| `CRATONVM_DBG_NETIF` | docs:1 |
| `CRATONVM_DBG_NEW` | docs:1 |
| `CRATONVM_DBG_NEWRESOLVE` | docs:1 |
| `CRATONVM_DBG_NIO` | docs:1 |
| `CRATONVM_DBG_NO_CONC_GC` | docs:2 |
| `CRATONVM_DBG_REFLECT` | docs:1 |
| `CRATONVM_DBG_SCAN_TIMING` | docs:1 |
| `CRATONVM_DBG_SCBUF` | docs:1 |
| `CRATONVM_DBG_SKIP_STRINGGC` | docs:1 |
| `CRATONVM_DBG_SOCK_ID` | docs:1 |
| `CRATONVM_DBG_SOCK_ID_FILE` | docs:1 |
| `CRATONVM_DBG_SOFTREF` | docs:1 |
| `CRATONVM_DBG_SPOOP` | docs:1 |
| `CRATONVM_DBG_STDIN_READ` | docs:1 |
| `CRATONVM_DBG_STREAM_MAP` | docs:1 |
| `CRATONVM_DBG_SWREENTER` | docs:1 |
| `CRATONVM_DBG_TABLEFILTER` | docs:2 |
| `CRATONVM_DBG_TLS_CIPHERS` | docs:2 |
| `CRATONVM_DBG_UPDATE` | docs:1 |
| `CRATONVM_DBG_VERIFY_TRUSTED_ROOTS` | docs:3 |
| `CRATONVM_DBG_WFBOOT` | docs:2 |
| `CRATONVM_DBG_ZIPFIELD` | docs:2 |

### Other (50)

| Flag | Referenced from |
| --- | --- |
| `CRATONVM_ALLOW_APP_SHIMS` | docs:1 |
| `CRATONVM_BUGS` | docs:29 |
| `CRATONVM_CRASHES` | docs:12 |
| `CRATONVM_CRASH_PROBE` | docs:2 |
| `CRATONVM_ENABLE_UNSAFE_INLINE_TLAB_NEW` | docs:2 |
| `CRATONVM_FORCE_MOVING` | docs:1 |
| `CRATONVM_G1_FORCE_COPY_CRITICAL` | docs:1 |
| `CRATONVM_JIT_FREE_CODE` | docs:8 |
| `CRATONVM_JIT_GUARDED_GETFIELD` | docs:10 |
| `CRATONVM_JIT_INLINE_PUTFIELD` | docs:5 |
| `CRATONVM_JIT_NO_INLINE_VCACHE` | docs:2 |
| `CRATONVM_JIT_NO_PREWARM_NEW` | docs:1 |
| `CRATONVM_JIT_SCAN_CACHE` | docs:1 |
| `CRATONVM_KEEP_SCRIPT` | docs:2 |
| `CRATONVM_LEGACY_NET_SOCKETS` | docs:1 |
| `CRATONVM_LENIENT_BOOT` | docs:1 |
| `CRATONVM_LOG` | docs:1 |
| `CRATONVM_MODULES_MARKER` | docs:1 |
| `CRATONVM_MSC_DBG` | docs:3 |
| `CRATONVM_NONMOVING_YOUNG` | docs:1 |
| `CRATONVM_NO_GC` | docs:4,scripts:2 |
| `CRATONVM_NO_GC_PROMOTION` | docs:3 |
| `CRATONVM_NO_STACK_BANG` | docs:1 |
| `CRATONVM_OSR_EXIT_TRANSFER` | docs:1 |
| `CRATONVM_POOL_DEBUG` | docs:1 |
| `CRATONVM_PRECISE_INLINE_FRAME_RECORD` | docs:2 |
| `CRATONVM_PRECISE_JIT_MAPS` | docs:36 |
| `CRATONVM_PTX_DUMP` | docs:1 |
| `CRATONVM_REAL_ARC` | docs:4 |
| `CRATONVM_REAL_INFINISPAN` | docs:1 |
| `CRATONVM_REAL_MSC` | docs:1 |
| `CRATONVM_REAL_SPRING_STARTUP` | docs:6,vm:1 |
| `CRATONVM_SELECTIVE_PROMOTE` | docs:13 |
| `CRATONVM_SHADOW_MARK` | docs:1 |
| `CRATONVM_SHADOW_OSR_TRACK` | docs:14 |
| `CRATONVM_SKIP_JBOSS_PLUMBING` | docs:3 |
| `CRATONVM_SKIP_REDUNDANT_NATIVE_SNAPSHOT` | docs:5 |
| `CRATONVM_SP_TIME` | docs:1 |
| `CRATONVM_STACK` | docs:1 |
| `CRATONVM_STACK_BANG_MARGIN_KB` | docs:1 |
| `CRATONVM_SWEEP_FULL_OLD_SCAN` | docs:1 |
| `CRATONVM_SYNTHETIC_JCA` | docs:2 |
| `CRATONVM_SYNTHETIC_SPRING_STARTUP` | docs:4 |
| `CRATONVM_TRUST_TAGGED_STACK_ROOTS` | docs:2 |
| `CRATONVM_UNBAN_SKIPSTRING` | docs:3 |
| `CRATONVM_UNROLL_UNSAFE_BODIES` | CHANGELOG.md:1 |
| `CRATONVM_USE_KC16_MAIN_SHIM` | docs:1 |
| `CRATONVM_USE_WILDFLY_MAIN_SHIM` | docs:2 |
| `CRATONVM_WILDFLY` | docs:1 |
| `CRATONVM_WILDFLY_SHORTCIRCUIT` | docs:2 |

## 3. Class (d) highlights — documented flags that do nothing

These are worth calling out separately because a reader of the docs would
reasonably believe they work, and at least two of them make a *measurement* wrong.

| Flag | Doc references | Reality |
| --- | ---: | --- |
| `CRATONVM_PRECISE_JIT_MAPS` | 36 | **No-op.** `jit/src/x64.rs:2078 precise_jit_maps_enabled()` reads the *inverse* flag `CRATONVM_NO_PRECISE_JIT_MAPS`. The doc comment at `x64.rs:2072` still says "Opt back in with `CRATONVM_PRECISE_JIT_MAPS=1`", which has not been true since the default was flipped. `x64.rs:2427` also warns against combining with a flag that cannot be set. |
| `CRATONVM_NO_GC` | 4 docs + 2 script uses | **No-op.** `scripts/measure-gc-fraction.sh:27-28` runs a `craton-nogc` arm with `CRATONVM_NO_GC=1`; nothing reads it, so that arm is identical to the baseline arm and any GC-fraction number derived from it is meaningless. |
| `CRATONVM_JIT_GUARDED_GETFIELD` | 10 | **No-op.** The gate is `guarded_inline_getfield_enabled()` at `jit/src/x64.rs:2224`, which reads `CRATONVM_JIT_GETFIELD_HELPER` with inverted polarity (set it to force the *helper*, i.e. disable the guarded inline path). |
| `CRATONVM_JIT_INLINE_PUTFIELD` | 5 | **No-op.** Real gate is `CRATONVM_NO_JIT_INLINE_PUTFIELD` (`x64.rs:2126`), opt-out. |
| `CRATONVM_SELECTIVE_PROMOTE` | 13 | **No-op.** Real gate is `CRATONVM_NO_SELECTIVE_PROMOTE` (`gc/src/gen_heap.rs:5803`), opt-out. |
| `CRATONVM_PRECISE_INLINE_FRAME_RECORD` | 2 | **No-op.** Real gate is `CRATONVM_NO_PRECISE_INLINE_FRAME_RECORD` (`x64.rs:2288`), opt-out. |
| `CRATONVM_SHADOW_OSR_TRACK` | 14 | **No-op.** No read site; the surviving shadow-stack knobs are `CRATONVM_SHADOW_STACK` / `_PIN` / `_NOPUSH` / `_NORELOAD`. |
| `CRATONVM_JIT_SCAN_CACHE` | 1 | **No-op.** Real gate is `CRATONVM_NO_JIT_SCAN_CACHE` (`vm/src/jit/conservative_roots.rs:899`). |
| `CRATONVM_NONMOVING_YOUNG` / `CRATONVM_FORCE_MOVING` | 1 each | **No-op.** Real gates are `CRATONVM_MOVING_YOUNG` and `CRATONVM_ALLOW_MOVING_YOUNG`. |
| `CRATONVM_BUGS` / `CRATONVM_CRASHES` | 29 / 12 | Not flags at all — doc-internal shorthand that the scanner picks up. Harmless, listed for completeness. |

The recurring pattern is a default flip: a flag `X` is introduced opt-in, later
made the default, and a new `NO_X` opt-out is added — but the docs keep describing
`X`. **No default is changed in this branch**; these are documentation bugs, and
are fixed as documentation.

## 4. Class (b) — semantics-changing flags

Every flag below selects a different execution path. These are the flags that
must be A/B verified by the migration: set and unset must behave exactly as they
did before the refactor.

`Polarity` is derived from the read expression: `is_some()`/`is_ok()` means opt-in
(default OFF), `is_none()`/`is_err()` means opt-out (default ON, so *deleting the
read would silently turn the feature off*).

| Flag | Reads | Cached | Polarity | Crates | First site |
| --- | ---: | :---: | --- | --- | --- |
| `CRATONVM_ALLOW_JSR_RET` | 2 | partial | value/other | classloading | `classloading/src/verifier.rs:169` |
| `CRATONVM_ALLOW_MOVING_YOUNG` | 2 | **no** | opt-in (default OFF) | gc | `gc/src/gen_heap.rs:3323` |
| `CRATONVM_AOT_HMAC_KEY` | 1 | **no** | value/other | native-builtins | `native-builtins/src/aot.rs:265` |
| `CRATONVM_ASYNC_HANDOFF_SLEEP_FLOOR_MS` | 1 | **no** | value/other | native-builtins | `native-builtins/src/lib.rs:71210` |
| `CRATONVM_ASYNC_SUBMIT_GRACE_MS` | 1 | yes | value/other | native-builtins | `native-builtins/src/lib.rs:71181` |
| `CRATONVM_ASYNC_WORKER_SLEEP_FLOOR_MS` | 1 | **no** | value/other | native-builtins | `native-builtins/src/lib.rs:71217` |
| `CRATONVM_AWAIT_NO_SHORTCIRCUIT` | 1 | **no** | opt-in (default OFF) | native-builtins | `native-builtins/src/wildfly_core.rs:1744` |
| `CRATONVM_BG_COMPILE` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:464` |
| `CRATONVM_BLOCK_PRIVATE_NETS` | 1 | **no** | value/other | native-io | `native-io/src/outbound_policy.rs:282` |
| `CRATONVM_BOOT_MODULE_REGISTRY` | 1 | **no** | value/other | classloading | `classloading/src/class_manager.rs:1743` |
| `CRATONVM_C2_SUPERSEDE` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:777` |
| `CRATONVM_CANON_OPENFILE` | 1 | **no** | opt-in (default OFF) | native-builtins | `native-builtins/src/phases_late.rs:16243` |
| `CRATONVM_CARD_TABLE_ONLY` | 1 | yes | opt-in (default OFF) | gc | `gc/src/gen_heap.rs:8910` |
| `CRATONVM_CL_BOOTSTRAP_SCOPED` | 1 | yes | value/other | native-builtins | `native-builtins/src/classloader.rs:1030` |
| `CRATONVM_COMPACT_REF_FIELDS` | 1 | yes | value/other | types | `types/src/field_layout.rs:175` |
| `CRATONVM_COMPRESSED_OOPS` | 1 | **no** | value/other | vm | `vm/src/vm/vm_init.rs:859` |
| `CRATONVM_CONFINE_IO` | 1 | **no** | value/other | native-io | `native-io/src/lib.rs:180` |
| `CRATONVM_DEFAULT_HEAP_ERGONOMICS` | 1 | **no** | value/other | vm-cli | `vm-cli/src/main.rs:3878` |
| `CRATONVM_DEFAULT_HEAP_MAX_MB` | 1 | **no** | value/other | vm-cli | `vm-cli/src/main.rs:3881` |
| `CRATONVM_DEFAULT_WATCHDOG_SEC` | 1 | **no** | value/other | vm-cli | `vm-cli/src/main.rs:2228` |
| `CRATONVM_DEOPT_EAGER` | 1 | yes | opt-in (default OFF) | jit | `jit/src/lib.rs:1013` |
| `CRATONVM_DEOPT_EAGER_BCI` | 1 | yes | value/other | jit | `jit/src/lib.rs:1027` |
| `CRATONVM_DEOPT_REAL` | 1 | yes | value/other | jit | `jit/src/lib.rs:967` |
| `CRATONVM_DISABLE_AALOAD_LICM` | 1 | **no** | opt-in (default OFF) | jit | `jit/src/x64.rs:28059` |
| `CRATONVM_DISABLE_ARITH_LICM` | 1 | **no** | opt-in (default OFF) | jit | `jit/src/x64.rs:28088` |
| `CRATONVM_DISABLE_DEFAULT_WATCHDOG` | 1 | **no** | value/other | vm-cli | `vm-cli/src/main.rs:2221` |
| `CRATONVM_DISABLE_INTRINSICS` | 3 | partial | value/other | difftest,vm | `vm/src/runtime/env_cache.rs:284` |
| `CRATONVM_DISABLE_JAR_MMAP` | 1 | **no** | opt-in (default OFF) | classloading | `classloading/src/class_path.rs:132` |
| `CRATONVM_DISABLE_JIT` | 5 | partial | value/other | difftest,vm | `vm/tests/wave2_bc_probe.rs:296` |
| `CRATONVM_DISABLE_SCALAR_REPLACEMENT` | 1 | **no** | opt-in (default OFF) | jit | `jit/src/x64.rs:28365` |
| `CRATONVM_DISABLE_UNROLL` | 1 | **no** | opt-in (default OFF) | jit | `jit/src/x64.rs:28214` |
| `CRATONVM_EAGER_STREAMS` | 1 | yes | opt-out (default ON) | native-collections | `native-collections/src/lib.rs:13160` |
| `CRATONVM_ENABLE_NATIVE_RING` | 1 | **no** | value/other | vm-cli | `vm-cli/src/main.rs:2220` |
| `CRATONVM_EQE_SYNC_EXECUTE` | 1 | **no** | opt-in (default OFF) | native-builtins | `native-builtins/src/wildfly_core.rs:1611` |
| `CRATONVM_EXEC_DEPTH_CEILING` | 1 | **no** | value/other | vm | `vm/src/runtime/interpreter.rs:4823` |
| `CRATONVM_FORCE_WIN_BUILD` | 1 | **no** | value/other | vm | `vm/src/vm/vm_init.rs:180` |
| `CRATONVM_FOREIGN_ATTACH` | 1 | **no** | value/other | vm | `vm/src/native/jni.rs:761` |
| `CRATONVM_FUZZ_BOOTCP` | 1 | **no** | value/other | fuzz | `fuzz/fuzz_targets/fuzz_verifier.rs:55` |
| `CRATONVM_G1_NO_EVAC_RETRY` | 1 | **no** | opt-in (default OFF) | gc | `gc/src/g1.rs:1879` |
| `CRATONVM_G1_PARALLEL_EVAC` | 1 | yes | value/other | gc | `gc/src/g1.rs:223` |
| `CRATONVM_G1_WORKERS` | 1 | **no** | value/other | gc | `gc/src/g1.rs:2886` |
| `CRATONVM_GC_OVERHEAD_LIMIT` | 1 | yes | value/other | vm | `vm/src/runtime/interpreter.rs:1637` |
| `CRATONVM_GC_STRESS` | 1 | yes | value/other | gc | `gc/src/gen_heap.rs:278` |
| `CRATONVM_GPU_NO_ZEROCOPY` | 1 | yes | opt-out (default ON) | vm | `vm/src/runtime/gpu_marshal.rs:689` |
| `CRATONVM_HARDEN_MANIFEST_CLASSPATH` | 1 | yes | value/other | classloading | `classloading/src/class_path.rs:223` |
| `CRATONVM_HELPFUL_NPE_OPCODES` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:326` |
| `CRATONVM_HTTP_MAX_BODY` | 1 | yes | value/other | native-builtins | `native-builtins/src/net_phase_e.rs:10791` |
| `CRATONVM_INHERIT_THREAD_CCL` | 1 | **no** | value/other | native-builtins | `native-builtins/src/lang_system.rs:752` |
| `CRATONVM_INHERIT_TL_WORKAROUND` | 1 | **no** | value/other | native-builtins | `native-builtins/src/lang_system.rs:793` |
| `CRATONVM_INLINE_ALLOW_STATIC` | 1 | yes | opt-in (default OFF) | vm | `vm/src/runtime/env_cache.rs:814` |
| `CRATONVM_IR_DEOPT_RESUME` | 1 | yes | opt-in (default OFF) | vm | `vm/src/runtime/interpreter.rs:11621` |
| `CRATONVM_JBOSS_BRUTE_FORCE_JARS` | 1 | **no** | value/other | native-builtins | `native-builtins/src/jboss_module_loader.rs:1371` |
| `CRATONVM_JBOSS_MP_ROOT` | 8 | **no** | value/other | native-builtins | `native-builtins/src/lib.rs:82` |
| `CRATONVM_JIT_ALLOW_PACKAGES` | 2 | yes | value/other | jit,vm | `jit/src/lib.rs:5527` |
| `CRATONVM_JIT_C2_FIRST_CALL` | 1 | yes | opt-in (default OFF) | vm | `vm/src/runtime/interpreter.rs:4950` |
| `CRATONVM_JIT_CODE_CACHE_MAX_MB` | 1 | yes | value/other | jit | `jit/src/lib.rs:619` |
| `CRATONVM_JIT_DENY` | 1 | yes | value/other | jit | `jit/src/lib.rs:5514` |
| `CRATONVM_JIT_DIRECT_CALLEE_CALLS` | 1 | **no** | value/other | jit | `jit/src/lib.rs:5826` |
| `CRATONVM_JIT_DISABLE_INLINE_NEW` | 2 | **no** | opt-out (default ON) | jit | `jit/src/x64.rs:27232` |
| `CRATONVM_JIT_DISPATCH_CACHE_DIRECT_ENTRY` | 1 | yes | value/other | vm | `vm/src/jit/helpers.rs:72` |
| `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY` | 1 | yes | opt-in (default OFF) | vm | `vm/src/jit/helpers.rs:91` |
| `CRATONVM_JIT_DUPX_EAGER_CANON` | 1 | yes | opt-in (default OFF) | jit | `jit/src/x64.rs:1088` |
| `CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS` | 2 | yes | value/other | jit,vm | `jit/src/x64.rs:2723` |
| `CRATONVM_JIT_ENABLE_INLINE_NEW` | 2 | **no** | opt-in (default OFF) | jit | `jit/src/x64.rs:27235` |
| `CRATONVM_JIT_FULL_SELF_CALL_SPILL` | 1 | yes | opt-in (default OFF) | jit | `jit/src/x64.rs:2670` |
| `CRATONVM_JIT_GETFIELD_HELPER` | 1 | **no** | opt-out (default ON) | jit | `jit/src/x64.rs:2231` |
| `CRATONVM_JIT_INCLUSIVE_BCE` | 1 | yes | value/other | jit | `jit/src/x64.rs:6147` |
| `CRATONVM_JIT_INLINE_GETFIELD` | 1 | yes | opt-in (default OFF) | jit | `jit/src/x64.rs:2174` |
| `CRATONVM_JIT_INLINE_SELF_GUARD` | 1 | yes | value/other | jit | `jit/src/x64.rs:2249` |
| `CRATONVM_JIT_IR_CALL` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:784` |
| `CRATONVM_JIT_IR_CALL_SPECIAL` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:790` |
| `CRATONVM_JIT_IR_CALL_VIRTUAL` | 1 | yes | opt-in (default OFF) | vm | `vm/src/runtime/env_cache.rs:802` |
| `CRATONVM_JIT_IR_DIRECT_CALL` | 1 | **no** | value/other | jit | `jit/src/lib.rs:5819` |
| `CRATONVM_JIT_IR_FP` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:808` |
| `CRATONVM_JIT_IR_LONG` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:796` |
| `CRATONVM_JIT_IR_SELFREC_DIRECT` | 1 | **no** | value/other | jit | `jit/src/lib.rs:8790` |
| `CRATONVM_JIT_KERNEL_REG_LOCALS` | 1 | yes | value/other | jit | `jit/src/x64.rs:2767` |
| `CRATONVM_JIT_KERNEL_REG_OSR` | 1 | yes | value/other | jit | `jit/src/x64.rs:2845` |
| `CRATONVM_JIT_LICM` | 1 | yes | value/other | jit | `jit/src/ir_optimize.rs:78` |
| `CRATONVM_JIT_MAIN_INLINE` | 1 | **no** | value/other | vm | `vm/src/runtime/env_cache.rs:433` |
| `CRATONVM_JIT_NO_BCE` | 1 | **no** | opt-in (default OFF) | jit | `jit/src/x64.rs:28149` |
| `CRATONVM_JIT_NO_CALLEE_OOP_FLUSH` | 1 | yes | opt-out (default ON) | jit | `jit/src/x64.rs:2704` |
| `CRATONVM_JIT_NO_DUPX` | 1 | yes | opt-in (default OFF) | jit | `jit/src/x64.rs:1064` |
| `CRATONVM_JIT_NO_DUP_X1` | 1 | yes | opt-in (default OFF) | jit | `jit/src/x64.rs:1071` |
| `CRATONVM_JIT_NO_DUP_X2` | 1 | yes | opt-in (default OFF) | jit | `jit/src/x64.rs:1077` |
| `CRATONVM_JIT_NO_LONG_INTRINSICS` | 1 | **no** | opt-out (default ON) | jit | `jit/src/lib.rs:3357` |
| `CRATONVM_JIT_NO_SELF_CACHE_INHERIT` | 1 | yes | opt-out (default ON) | jit | `jit/src/x64.rs:2262` |
| `CRATONVM_JIT_NO_SLOT_MIRROR` | 1 | yes | opt-out (default ON) | jit | `jit/src/x64.rs:2867` |
| `CRATONVM_JIT_NO_SPEC_BCE` | 1 | yes | opt-in (default OFF) | jit | `jit/src/x64.rs:6159` |
| `CRATONVM_JIT_NO_STACK_BANG` | 1 | yes | opt-in (default OFF) | jit | `jit/src/x64.rs:180` |
| `CRATONVM_JIT_OSR` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:125` |
| `CRATONVM_JIT_RANGE_SCAN_LEGACY` | 1 | yes | opt-in (default OFF) | vm | `vm/src/jit/conservative_roots.rs:1030` |
| `CRATONVM_JIT_REASSOC` | 1 | yes | opt-in (default OFF) | jit | `jit/src/ir_optimize.rs:86` |
| `CRATONVM_JIT_SAFEPOINT_POLLS` | 2 | partial | value/other | jit | `jit/src/x64.rs:2595` |
| `CRATONVM_JIT_SAFEPOINT_REG_SPILL` | 3 | yes | opt-in (default OFF) | jit | `jit/src/x64.rs:2610` |
| `CRATONVM_JIT_SCALAR_NEW` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:764` |
| `CRATONVM_JIT_STACK_BANG` | 1 | yes | value/other | jit | `jit/src/x64.rs:183` |
| `CRATONVM_JIT_THRESHOLD` | 3 | partial | value/other | difftest,vm | `vm/src/runtime/env_cache.rs:88` |
| `CRATONVM_JIT_UNBAN_JUNITCORE` | 1 | **no** | opt-in (default OFF) | vm | `vm/src/jit/skip_list.rs:915` |
| `CRATONVM_JIT_UNROLL` | 1 | yes | value/other | jit | `jit/src/ir_optimize.rs:1905` |
| `CRATONVM_JIT_VIRTUAL_TIERUP` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:494` |
| `CRATONVM_LAZY_STREAMS` | 1 | yes | opt-in (default OFF) | native-collections | `native-collections/src/lib.rs:13157` |
| `CRATONVM_LENIENT_CLINIT` | 2 | partial | value/other | vm | `vm/src/vm/vm_util.rs:53` |
| `CRATONVM_LOADER_AWARE_RESOLUTION` | 1 | yes | value/other | classloading | `classloading/src/class_manager.rs:141` |
| `CRATONVM_LOADER_UNLOAD` | 2 | yes | value/other | native-builtins,types | `native-builtins/src/classloader.rs:1056` |
| `CRATONVM_LONGREWRITE_LOOSE` | 1 | yes | opt-in (default OFF) | vm | `vm/src/runtime/value_stack.rs:198` |
| `CRATONVM_MAVEN_REPO_LOCAL` | 2 | **no** | value/other | native-builtins | `native-builtins/src/jboss_module_loader.rs:301` |
| `CRATONVM_MAX_INFLATED_BYTES` | 1 | **no** | value/other | native-builtins | `native-builtins/src/phases_late.rs:19788` |
| `CRATONVM_MOVING_YOUNG` | 3 | yes | opt-in (default OFF) | gc,jit,vm | `jit/src/x64.rs:2458` |
| `CRATONVM_MOVING_YOUNG_FALLBACKS` | 1 | **no** | opt-in (default OFF) | gc | `gc/src/gen_heap.rs:3792` |
| `CRATONVM_MSC_REAL_START` | 1 | yes | value/other | native-builtins | `native-builtins/src/jboss_msc.rs:2599` |
| `CRATONVM_NATIVE_EC_MULTIPLY` | 1 | yes | opt-in (default OFF) | native-builtins | `native-builtins/src/sunec_point.rs:58` |
| `CRATONVM_NATIVE_MATCHER_FIND` | 2 | partial | value/other | native-builtins,vm | `native-builtins/src/lib.rs:26365` |
| `CRATONVM_NATIVE_PBE_KEYFACTORY` | 1 | **no** | value/other | native-builtins | `native-builtins/src/phases_early.rs:13317` |
| `CRATONVM_NATIVE_STRING_REGEX` | 2 | partial | value/other | native-builtins,vm | `native-builtins/src/lib.rs:26313` |
| `CRATONVM_NETTY_QUEUE_BRIDGE` | 1 | **no** | value/other | native-builtins | `native-builtins/src/lib.rs:20704` |
| `CRATONVM_NO_CONSERVATIVE_LOCALS` | 1 | yes | opt-out (default ON) | vm | `vm/src/memory/roots.rs:36` |
| `CRATONVM_NO_CTOR_DIRECT_CALL` | 1 | **no** | value/other | vm | `vm/src/runtime/env_cache.rs:587` |
| `CRATONVM_NO_GC_PROMOTION_GUARD` | 1 | **no** | opt-out (default ON) | gc | `gc/src/gen_heap.rs:3702` |
| `CRATONVM_NO_IR_BRANCHY` | 1 | yes | opt-out (default ON) | jit | `jit/src/ir_optimize.rs:98` |
| `CRATONVM_NO_JIT_ALLOC_CLASS_CACHE` | 1 | yes | opt-out (default ON) | vm | `vm/src/jit/alloc_class_cache.rs:218` |
| `CRATONVM_NO_JIT_INLINE_PUTFIELD` | 1 | yes | opt-out (default ON) | jit | `jit/src/x64.rs:2126` |
| `CRATONVM_NO_JIT_INLINE_TLAB_NEW` | 1 | yes | opt-out (default ON) | jit | `jit/src/x64.rs:2149` |
| `CRATONVM_NO_JIT_SCAN_CACHE` | 1 | **no** | opt-out (default ON) | vm | `vm/src/jit/conservative_roots.rs:899` |
| `CRATONVM_NO_LOCAL_LIVENESS` | 1 | **no** | value/other | vm | `vm/src/runtime/env_cache.rs:679` |
| `CRATONVM_NO_PRECISE_INLINE_FRAME_RECORD` | 1 | yes | opt-out (default ON) | jit | `jit/src/x64.rs:2288` |
| `CRATONVM_NO_PRECISE_JIT_MAPS` | 1 | yes | opt-out (default ON) | jit | `jit/src/x64.rs:2085` |
| `CRATONVM_NO_PRECISE_REG_SPILL` | 1 | yes | opt-in (default OFF) | jit | `jit/src/x64.rs:2661` |
| `CRATONVM_NO_SELECTIVE_PROMOTE` | 3 | **no** | opt-out (default ON) | difftest,gc | `difftest/src/runner.rs:179` |
| `CRATONVM_NO_STUBS` | 1 | **no** | value/other | native-api | `native-api/src/registry.rs:3886` |
| `CRATONVM_OLD_SWEEP_JIT` | 1 | **no** | value/other | gc | `gc/src/gen_heap.rs:3546` |
| `CRATONVM_OSR_EXIT_AFTER` | 1 | yes | value/other | jit | `jit/src/lib.rs:1062` |
| `CRATONVM_OSR_EXIT_TEST` | 1 | yes | opt-in (default OFF) | jit | `jit/src/lib.rs:1042` |
| `CRATONVM_OSR_NEWARRAY` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:266` |
| `CRATONVM_PRECISE_COVERAGE_PIN` | 1 | yes | opt-in (default OFF) | vm | `vm/src/jit/conservative_roots.rs:1913` |
| `CRATONVM_PROMOTION_OOM_GUARD_BROAD` | 1 | **no** | opt-in (default OFF) | gc | `gc/src/gen_heap.rs:3756` |
| `CRATONVM_REAL` | 2 | partial | value/other | vm | `vm/tests/synthetic_diff.rs:289` |
| `CRATONVM_REAL_AGROAL` | 1 | **no** | opt-in (default OFF) | native-builtins | `native-builtins/src/lib.rs:83081` |
| `CRATONVM_REAL_ANNOTATIONS` | 3 | partial | value/other | native-builtins,vm | `native-builtins/src/lang_class.rs:10381` |
| `CRATONVM_REAL_AQS` | 7 | **no** | opt-out (default ON) | native-builtins,vm | `native-builtins/src/lib.rs:26519` |
| `CRATONVM_REAL_FORKJOINPOOL` | 6 | partial | opt-out (default ON) | native-api,vm | `vm/tests/synthetic_diff.rs:294` |
| `CRATONVM_REAL_JCA` | 2 | partial | opt-in (default OFF) | native-builtins,vm | `native-builtins/src/lib.rs:8913` |
| `CRATONVM_REAL_NET_SOCKETS` | 8 | partial | opt-in (default OFF) | native-api,native-builtins,native-io,vm | `native-builtins/src/phases_early.rs:15080` |
| `CRATONVM_REAL_PROXY` | 1 | **no** | value/other | native-builtins | `native-builtins/src/lib.rs:83010` |
| `CRATONVM_REAL_PROXY_STRICT` | 1 | **no** | value/other | native-builtins | `native-builtins/src/lib.rs:83032` |
| `CRATONVM_REAL_PROXY_SUPER` | 3 | partial | opt-out (default ON) | native-builtins,vm | `native-builtins/src/lib.rs:83052` |
| `CRATONVM_REAL_QUARKUS_START` | 1 | **no** | opt-in (default OFF) | native-builtins | `native-builtins/src/quarkus_staticinit.rs:658` |
| `CRATONVM_REAL_STAX_FACTORY` | 1 | yes | value/other | native-builtins | `native-builtins/src/xml_stax.rs:747` |
| `CRATONVM_REAL_VERTX` | 1 | **no** | opt-in (default OFF) | native-builtins | `native-builtins/src/lib.rs:83095` |
| `CRATONVM_RECLAIM_DEAD_MONITORS` | 1 | yes | opt-in (default OFF) | vm | `vm/src/threading/monitor.rs:95` |
| `CRATONVM_REQUIRE_POLICY` | 1 | yes | opt-in (default OFF) | native-builtins | `native-builtins/src/security_manager.rs:61` |
| `CRATONVM_RESOLVE_CACHE_CAP` | 4 | **no** | value/other | vm | `vm/src/runtime/lockfree_resolve.rs:56` |
| `CRATONVM_RESOLVE_OUTBOUND_HOST` | 1 | **no** | value/other | native-io | `native-io/src/outbound_policy.rs:256` |
| `CRATONVM_ROOTSNAP_CACHE` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:389` |
| `CRATONVM_ROOTSNAP_CACHE_SURVIVE_GC` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:422` |
| `CRATONVM_SCALAR_DEOPT` | 1 | yes | opt-in (default OFF) | jit | `jit/src/lib.rs:987` |
| `CRATONVM_SELECT_MAX_BLOCK_MS` | 1 | yes | value/other | native-io | `native-io/src/nio_selector.rs:125` |
| `CRATONVM_SHADOW_NOPUSH` | 2 | partial | opt-in (default OFF) | jit,vm | `jit/src/x64.rs:2501` |
| `CRATONVM_SHADOW_NORELOAD` | 2 | partial | opt-in (default OFF) | jit,vm | `jit/src/x64.rs:2511` |
| `CRATONVM_SHADOW_NO_SAVEBASE` | 1 | yes | opt-in (default OFF) | jit | `jit/src/x64.rs:2583` |
| `CRATONVM_SHADOW_PIN` | 2 | yes | opt-in (default OFF) | jit,vm | `jit/src/x64.rs:2525` |
| `CRATONVM_SHADOW_RAW_RELOAD` | 1 | yes | opt-in (default OFF) | jit | `jit/src/x64.rs:2572` |
| `CRATONVM_SHADOW_STACK` | 4 | partial | opt-in (default OFF) | gc,jit,vm | `jit/src/x64.rs:2436` |
| `CRATONVM_SOFT_EXIT` | 5 | **no** | value/other | native-builtins | `native-builtins/src/lib.rs:44233` |
| `CRATONVM_SP_NO_COALESCE` | 1 | **no** | opt-out (default ON) | gc | `gc/src/gen_heap.rs:7533` |
| `CRATONVM_STRICT_JIT_ROOTS` | 1 | yes | opt-in (default OFF) | vm | `vm/src/jit/conservative_roots.rs:1223` |
| `CRATONVM_SYNTHETIC_AGROAL` | 1 | **no** | opt-out (default ON) | native-builtins | `native-builtins/src/lib.rs:83082` |
| `CRATONVM_SYNTHETIC_ANNOTATIONS` | 1 | yes | opt-in (default OFF) | native-builtins | `native-builtins/src/lang_class.rs:10377` |
| `CRATONVM_SYNTHETIC_AQS` | 5 | **no** | opt-in (default OFF) | native-builtins | `native-builtins/src/lib.rs:26518` |
| `CRATONVM_SYNTHETIC_BUFFERED_WRITER` | 1 | **no** | value/other | native-builtins | `native-builtins/src/phases_late.rs:11228` |
| `CRATONVM_SYNTHETIC_DSA` | 1 | yes | opt-out (default ON) | native-builtins | `native-builtins/src/lib.rs:8958` |
| `CRATONVM_SYNTHETIC_EC` | 1 | yes | opt-out (default ON) | native-builtins | `native-builtins/src/lib.rs:8939` |
| `CRATONVM_SYNTHETIC_EQE` | 1 | **no** | opt-in (default OFF) | native-builtins | `native-builtins/src/wildfly_core.rs:2061` |
| `CRATONVM_SYNTHETIC_FILEWRITER` | 1 | yes | value/other | native-io | `native-io/src/lib.rs:4569` |
| `CRATONVM_SYNTHETIC_PQC` | 1 | yes | opt-out (default ON) | native-builtins | `native-builtins/src/lib.rs:8975` |
| `CRATONVM_SYNTHETIC_QUARKUS_ARC` | 2 | **no** | value/other | native-builtins | `native-builtins/src/quarkus_arc.rs:374` |
| `CRATONVM_SYNTHETIC_RAF` | 2 | partial | value/other | native-builtins,native-io | `native-builtins/src/phases_late.rs:4856` |
| `CRATONVM_SYNTHETIC_RSA` | 1 | yes | opt-out (default ON) | native-builtins | `native-builtins/src/lib.rs:9006` |
| `CRATONVM_SYNTHETIC_VERTX` | 1 | **no** | opt-out (default ON) | native-builtins | `native-builtins/src/lib.rs:83096` |
| `CRATONVM_THREAD_START_GRACE_MS` | 1 | yes | value/other | vm | `vm/src/vm/vm_exec.rs:139` |
| `CRATONVM_TIER_C1_THRESHOLD` | 2 | **no** | value/other | jit | `jit/src/tiered.rs:211` |
| `CRATONVM_TIER_C2_MIN_INVOCATIONS` | 2 | **no** | value/other | jit | `jit/src/tiered.rs:220` |
| `CRATONVM_TIER_C2_THRESHOLD` | 2 | **no** | value/other | jit | `jit/src/tiered.rs:214` |
| `CRATONVM_TIER_ENABLED` | 2 | **no** | value/other | jit | `jit/src/tiered.rs:223` |
| `CRATONVM_TIER_OSR_BACKEDGE` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:236` |
| `CRATONVM_TIER_OSR_THRESHOLD` | 2 | **no** | value/other | jit | `jit/src/tiered.rs:217` |
| `CRATONVM_TIER_PGO` | 1 | **no** | value/other | vm | `vm/src/runtime/env_cache.rs:479` |
| `CRATONVM_TLAB_GC_TRIGGER` | 1 | yes | value/other | vm | `vm/src/runtime/interpreter.rs:2692` |
| `CRATONVM_TRUST_PEM` | 2 | **no** | value/other | classloading | `classloading/src/jar_signer.rs:1939` |
| `CRATONVM_UNTRUSTED_CODE` | 1 | **no** | value/other | native-io | `native-io/src/lib.rs:181` |
| `CRATONVM_URI_STRICT_CHARS` | 2 | **no** | value/other | native-builtins | `native-builtins/src/lib.rs:73154` |
| `CRATONVM_USE_WILDFLY_REFLECT_SHIM` | 1 | **no** | value/other | native-builtins | `native-builtins/src/lang_class.rs:8082` |
| `CRATONVM_USE_WILDFLY_SYNTH_BYTECODE` | 2 | **no** | value/other | native-builtins | `native-builtins/src/lang_class.rs:1602` |
| `CRATONVM_WEAKREF_CLEAR` | 1 | yes | value/other | vm | `vm/src/runtime/interpreter.rs:148` |
| `CRATONVM_XT_HELPER_WINDOW_SCAN` | 1 | **no** | value/other | vm | `vm/src/jit/xt_root_scan.rs:127` |
| `CRATONVM_XT_JIT_ROOT_SCAN` | 2 | partial | value/other | jit,vm | `jit/src/lib.rs:879` |
| `CRATONVM_YOUNGSCAN_STRIDE` | 1 | yes | value/other | vm | `vm/src/vm/vm_exec.rs:668` |
| `CRATONVM_ZIP_MAX_ENTRY_BYTES` | 1 | yes | value/other | native-io | `native-io/src/zip_real_jar.rs:88` |

## 5. Class (a) — debug / diagnostic flags

340 flags. These gate `eprintln!` / `tracing` output or extra assertions
only. They are the bulk of the surface and the best candidates for collapsing
behind a single `experimental-diag` cargo feature: with the feature off the
accessor becomes `const false` and the whole diagnostic block folds away.

| Flag | Reads | Cached | Crates |
| --- | ---: | :---: | --- |
| `CRATONVM_DBG_TLS_AUTH` | 41 | no | native-builtins |
| `CRATONVM_DBG_LOADER_TRACE` | 33 | no | native-builtins,vm |
| `CRATONVM_DBG_SBLOAD` | 18 | partial | native-builtins,native-collections |
| `CRATONVM_DBG_DEOPT` | 17 | no | jit,vm |
| `CRATONVM_DBG_TLS_HS` | 15 | no | native-builtins |
| `CRATONVM_DBG_TLS_SOCK` | 14 | no | native-builtins |
| `CRATONVM_IAE_TRACE` | 12 | partial | native-builtins,vm |
| `CRATONVM_DBG_A2` | 8 | partial | gc,vm |
| `CRATONVM_DBG_BLOCKGC` | 8 | partial | vm |
| `CRATONVM_DBG_SCALAR_DEOPT` | 8 | no | jit,vm |
| `CRATONVM_DBG_STTRACE` | 8 | no | native-builtins,vm |
| `CRATONVM_DBG_TOARRAY` | 8 | partial | native-builtins,native-collections,vm |
| `CRATONVM_G1_DBG_REACH` | 8 | no | gc |
| `CRATONVM_DBG_ARGS` | 7 | no | vm-cli |
| `CRATONVM_DBG_ASSERTJ_ARR` | 7 | no | native-builtins |
| `CRATONVM_DBG_MH_DISPATCH` | 6 | no | native-builtins |
| `CRATONVM_DBG_NIO_BIND` | 6 | no | native-builtins |
| `CRATONVM_DBG_OBSREG` | 6 | no | classloading,native-builtins |
| `CRATONVM_DBG_BUG03` | 5 | no | vm |
| `CRATONVM_DBG_DEFLATE` | 5 | no | native-builtins |
| `CRATONVM_DBG_MIRRORPIN` | 5 | no | gc,native-builtins,native-collections,vm |
| `CRATONVM_DBG_XNIO_TCP` | 5 | no | native-builtins |
| `CRATONVM_DEBUG_STACKWALK` | 5 | no | native-builtins |
| `CRATONVM_DIAG_SERVICELOADER` | 5 | no | native-builtins |
| `CRATONVM_DBG_FBCGLIB` | 4 | no | classloading,native-builtins |
| `CRATONVM_DBG_FIELD_WATCH` | 4 | partial | types,vm |
| `CRATONVM_DBG_GETRESOURCES` | 4 | partial | classloading,native-builtins |
| `CRATONVM_DBG_JITC` | 4 | no | jit,vm |
| `CRATONVM_DBG_JIT_GEN` | 4 | no | jit |
| `CRATONVM_DBG_LAMBDA_GENERIC` | 4 | no | native-builtins |
| `CRATONVM_DBG_NET` | 4 | no | native-builtins,native-io |
| `CRATONVM_DBG_OBJECTS` | 4 | no | native-builtins |
| `CRATONVM_DBG_OOBFIELD` | 4 | no | gc |
| `CRATONVM_DBG_RBC6` | 4 | partial | jit,vm |
| `CRATONVM_DBG_REFLECTION_FACTORY` | 4 | no | native-builtins,vm |
| `CRATONVM_DBG_RETRANSFORM` | 4 | no | vm |
| `CRATONVM_DBG_SEL` | 4 | no | native-builtins |
| `CRATONVM_DBG_STW_CENSUS` | 4 | no | vm |
| `CRATONVM_DBG_WATCHREF` | 4 | partial | gc,vm |
| `CRATONVM_DBG_WF` | 4 | no | native-builtins |
| `CRATONVM_ANN_TRACE` | 3 | no | native-builtins |
| `CRATONVM_DBG_ALTRACE` | 3 | partial | native-collections,vm |
| `CRATONVM_DBG_CCE_BT` | 3 | partial | native-builtins,native-collections,vm |
| `CRATONVM_DBG_COMPACT_INLINE` | 3 | no | jit |
| `CRATONVM_DBG_EXIT` | 3 | no | native-builtins,vm-cli |
| `CRATONVM_DBG_FBREF` | 3 | no | native-builtins |
| `CRATONVM_DBG_FORCE_MOVING` | 3 | no | gc,vm |
| `CRATONVM_DBG_H2TRACE` | 3 | no | native-builtins,vm |
| `CRATONVM_DBG_JETTY` | 3 | no | native-io,vm |
| `CRATONVM_DBG_NULLTHIS` | 3 | no | vm |
| `CRATONVM_DBG_PB` | 3 | no | native-builtins,native-io |
| `CRATONVM_DBG_PRECISE` | 3 | no | gc,vm |
| `CRATONVM_DBG_SLEEP_TRACE` | 3 | no | native-builtins |
| `CRATONVM_DBG_SOCK` | 3 | no | native-builtins |
| `CRATONVM_DBG_STRAYSTACK` | 3 | partial | vm |
| `CRATONVM_DBG_SWEEP_CENSUS` | 3 | no | gc |
| `CRATONVM_DBG_TLABMISS` | 3 | yes | vm |
| `CRATONVM_DBG_TLS_SRV` | 3 | no | native-builtins |
| `CRATONVM_DBG_UCLRES` | 3 | no | native-builtins |
| `CRATONVM_DBG_UNROLL` | 3 | no | jit |
| `CRATONVM_DBG_XT_JIT_ROOT_SCAN` | 3 | no | vm |
| `CRATONVM_TRACE_CLASSVALUE` | 3 | partial | native-builtins,vm |
| `CRATONVM_BD_DEBUG` | 2 | no | native-builtins,vm |
| `CRATONVM_DBG_AIOOBE` | 2 | partial | vm |
| `CRATONVM_DBG_BADREF` | 2 | no | gc |
| `CRATONVM_DBG_CATALINA` | 2 | no | native-builtins,vm |
| `CRATONVM_DBG_CAUSE` | 2 | no | native-builtins |
| `CRATONVM_DBG_CORRUPT_FRAMES` | 2 | no | vm |
| `CRATONVM_DBG_CTOR_FIX` | 2 | no | vm |
| `CRATONVM_DBG_EQE` | 2 | no | native-builtins |
| `CRATONVM_DBG_EXEC` | 2 | no | native-builtins,native-collections |
| `CRATONVM_DBG_HMPUT` | 2 | partial | native-collections |
| `CRATONVM_DBG_HTTPSRV` | 2 | no | native-builtins |
| `CRATONVM_DBG_JAR` | 2 | no | native-io |
| `CRATONVM_DBG_JLM` | 2 | no | native-builtins |
| `CRATONVM_DBG_LAMBDA_DISPATCH` | 2 | no | vm |
| `CRATONVM_DBG_LETSGO` | 2 | no | vm |
| `CRATONVM_DBG_LINKER` | 2 | no | native-builtins |
| `CRATONVM_DBG_LOOKUP` | 2 | no | native-builtins |
| `CRATONVM_DBG_NEXTINT` | 2 | no | native-builtins,native-collections |
| `CRATONVM_DBG_OSR` | 2 | no | vm |
| `CRATONVM_DBG_PICOCLI_STYLE` | 2 | no | native-builtins |
| `CRATONVM_DBG_RAF_GETFD` | 2 | no | native-builtins |
| `CRATONVM_DBG_SCALAR_NEW` | 2 | no | jit |
| `CRATONVM_DBG_SC_READ` | 2 | no | native-io |
| `CRATONVM_DBG_SC_WRITE` | 2 | no | native-io |
| `CRATONVM_DBG_SEEDHUNT` | 2 | yes | gc |
| `CRATONVM_DBG_SHADOW` | 2 | no | vm |
| `CRATONVM_DBG_SOCK_BYTES` | 2 | no | native-builtins |
| `CRATONVM_DBG_SPID` | 2 | no | jit |
| `CRATONVM_DBG_STALE_OBJREF` | 2 | partial | gc |
| `CRATONVM_DBG_STW_EXPECTED_IDS` | 2 | no | vm |
| `CRATONVM_DBG_TLS_PLS` | 2 | no | native-builtins |
| `CRATONVM_DBG_VDISP` | 2 | partial | native-builtins,vm |
| `CRATONVM_DBG_YOUNGSTATE` | 2 | no | gc |
| `CRATONVM_GC_VERIFY_STALE` | 2 | partial | vm |
| `CRATONVM_INVOKE_VIRTUAL_ENTRY_TRACE` | 2 | no | vm |
| `CRATONVM_SFI_NULL_TRACE` | 2 | no | native-builtins |
| `CRATONVM_TRACE_UNIMPLEMENTED` | 2 | partial | classloading,vm |
| `CRATONVM_ACTIVE_PROFILES_IDENTITY_TRACE` | 1 | no | vm |
| `CRATONVM_ANN_PROXY_DISPATCH_TRACE` | 1 | no | vm |
| `CRATONVM_ASSERT_SINGLE_OS_THREAD` | 1 | no | types |
| `CRATONVM_DBG` | 1 | no | native-builtins |
| `CRATONVM_DBG_ACCESS` | 1 | no | classloading |
| `CRATONVM_DBG_AIO` | 1 | yes | native-io |
| `CRATONVM_DBG_AIOOBE2` | 1 | yes | vm |
| `CRATONVM_DBG_AIOOBE3` | 1 | yes | vm |
| `CRATONVM_DBG_ANNPROXY_WRAP` | 1 | no | native-builtins |
| `CRATONVM_DBG_ANONALLOC` | 1 | yes | vm |
| `CRATONVM_DBG_AQS_TRACE` | 1 | yes | native-builtins |
| `CRATONVM_DBG_ARRAYCOPY` | 1 | no | native-builtins |
| `CRATONVM_DBG_ARRLEN` | 1 | no | vm |
| `CRATONVM_DBG_ARRSTORE` | 1 | yes | vm |
| `CRATONVM_DBG_ASSERTEQ` | 1 | no | vm |
| `CRATONVM_DBG_ATHROW` | 1 | no | vm |
| `CRATONVM_DBG_ATOMIC_UPDATER` | 1 | no | native-builtins |
| `CRATONVM_DBG_BADRECV` | 1 | no | vm |
| `CRATONVM_DBG_BB` | 1 | yes | native-builtins |
| `CRATONVM_DBG_BBLP` | 1 | no | vm |
| `CRATONVM_DBG_BLOCKED_ACCESS` | 1 | yes | gc |
| `CRATONVM_DBG_BUFUNDER` | 1 | no | vm |
| `CRATONVM_DBG_BYTECODE_DUMP` | 1 | no | vm |
| `CRATONVM_DBG_CALLER` | 1 | no | native-builtins |
| `CRATONVM_DBG_CAPVAL` | 1 | no | native-builtins |
| `CRATONVM_DBG_CCE` | 1 | no | vm |
| `CRATONVM_DBG_CCECACHE` | 1 | no | native-builtins |
| `CRATONVM_DBG_CCSPROBE` | 1 | no | vm |
| `CRATONVM_DBG_CELLCORRUPT` | 1 | yes | gc |
| `CRATONVM_DBG_CHARSET` | 1 | no | vm |
| `CRATONVM_DBG_CLASSPATH` | 1 | no | classloading |
| `CRATONVM_DBG_CLONE` | 1 | yes | native-builtins |
| `CRATONVM_DBG_COERCE` | 1 | no | native-builtins |
| `CRATONVM_DBG_COMPACTVALUE` | 1 | no | types |
| `CRATONVM_DBG_COMPACT_LEGACY` | 1 | no | gc |
| `CRATONVM_DBG_COMPONENT_TYPE` | 1 | no | native-builtins |
| `CRATONVM_DBG_DEFINE` | 1 | no | classloading |
| `CRATONVM_DBG_DESCTRACE` | 1 | yes | gc |
| `CRATONVM_DBG_DOPRIV` | 1 | yes | native-builtins |
| `CRATONVM_DBG_DROPPED_STUBS` | 1 | no | native-api |
| `CRATONVM_DBG_DUMP_JIT` | 1 | no | jit |
| `CRATONVM_DBG_DUPCALL_FILTER` | 1 | no | vm |
| `CRATONVM_DBG_DUPCLASS` | 1 | no | classloading |
| `CRATONVM_DBG_DUPCLASS_BT` | 1 | no | classloading |
| `CRATONVM_DBG_DUPX_METHODS` | 1 | no | jit |
| `CRATONVM_DBG_ECWATCH` | 1 | yes | vm |
| `CRATONVM_DBG_ECWATCH_NATIVE` | 1 | yes | vm |
| `CRATONVM_DBG_FIELDADDR` | 1 | no | vm |
| `CRATONVM_DBG_FIELD_GET` | 1 | no | native-builtins |
| `CRATONVM_DBG_FSP` | 1 | no | native-builtins |
| `CRATONVM_DBG_FULLSTACK_SCAN` | 1 | yes | vm |
| `CRATONVM_DBG_FWDGUARD` | 1 | yes | gc |
| `CRATONVM_DBG_GCPART` | 1 | yes | vm |
| `CRATONVM_DBG_GCPAUSE` | 1 | yes | gc |
| `CRATONVM_DBG_GCPHASE` | 1 | no | gc |
| `CRATONVM_DBG_GCWRITE` | 1 | yes | gc |
| `CRATONVM_DBG_GC_OVERHEAD` | 1 | no | vm |
| `CRATONVM_DBG_GC_STRESS` | 1 | yes | gc |
| `CRATONVM_DBG_GOCBF` | 1 | no | native-builtins |
| `CRATONVM_DBG_HANGWALK` | 1 | no | vm |
| `CRATONVM_DBG_HANG_SAMPLE` | 1 | no | vm |
| `CRATONVM_DBG_HEAPCOPY` | 1 | no | vm |
| `CRATONVM_DBG_HEAP_STALE` | 1 | no | vm |
| `CRATONVM_DBG_HEAP_TRACE` | 1 | no | gc |
| `CRATONVM_DBG_HEARTBEAT` | 1 | no | vm |
| `CRATONVM_DBG_HOTPATH_COUNTS` | 1 | no | vm |
| `CRATONVM_DBG_IMSE` | 1 | no | vm |
| `CRATONVM_DBG_INDY_ALL` | 1 | no | vm |
| `CRATONVM_DBG_INDY_GENERIC` | 1 | no | vm |
| `CRATONVM_DBG_INLINE_FR` | 1 | no | jit |
| `CRATONVM_DBG_INVOKESTATS` | 1 | yes | vm |
| `CRATONVM_DBG_INVOKE_COERCE` | 1 | no | native-builtins |
| `CRATONVM_DBG_IRSLOT` | 1 | no | jit |
| `CRATONVM_DBG_IR_CALL` | 1 | no | jit |
| `CRATONVM_DBG_IR_LONG` | 1 | no | jit |
| `CRATONVM_DBG_ISINSTANCE` | 1 | no | native-builtins |
| `CRATONVM_DBG_JETTY2` | 1 | no | vm |
| `CRATONVM_DBG_JIT_ALLOC` | 1 | yes | vm |
| `CRATONVM_DBG_JIT_CODE` | 1 | no | jit |
| `CRATONVM_DBG_JIT_DISASM` | 1 | yes | vm |
| `CRATONVM_DBG_JIT_DISPATCH` | 1 | no | vm |
| `CRATONVM_DBG_JIT_ENTRY` | 1 | no | vm |
| `CRATONVM_DBG_JIT_LDC` | 1 | no | vm |
| `CRATONVM_DBG_JIT_METHOD_STATS` | 1 | no | vm-cli |
| `CRATONVM_DBG_JIT_MIC` | 1 | no | vm |
| `CRATONVM_DBG_JIT_NAMES` | 1 | yes | jit |
| `CRATONVM_DBG_JIT_PUTFIELD` | 1 | no | vm |
| `CRATONVM_DBG_JIT_SAFEPOINTS` | 1 | no | vm |
| `CRATONVM_DBG_KCBOOL` | 1 | yes | native-collections |
| `CRATONVM_DBG_LAMBDA` | 1 | no | vm |
| `CRATONVM_DBG_LAYOUT` | 1 | no | classloading |
| `CRATONVM_DBG_LHM_EVICT` | 1 | no | native-collections |
| `CRATONVM_DBG_LICM` | 1 | no | jit |
| `CRATONVM_DBG_LOADCLASS` | 1 | no | classloading |
| `CRATONVM_DBG_LOGPROV` | 1 | no | native-builtins |
| `CRATONVM_DBG_LONGROOT` | 1 | yes | vm |
| `CRATONVM_DBG_MCL` | 1 | no | native-builtins |
| `CRATONVM_DBG_MEMWATCH` | 1 | yes | vm |
| `CRATONVM_DBG_METHOD_INVOKE_BOX` | 1 | yes | native-builtins |
| `CRATONVM_DBG_MH_ADAPTER` | 1 | no | vm |
| `CRATONVM_DBG_MH_STACK` | 1 | no | vm |
| `CRATONVM_DBG_MIC_PROF` | 1 | yes | vm |
| `CRATONVM_DBG_MINVOKE` | 1 | no | native-builtins |
| `CRATONVM_DBG_MODPROV` | 1 | no | classloading |
| `CRATONVM_DBG_MODSTATIC` | 1 | no | vm |
| `CRATONVM_DBG_MONENTER` | 1 | yes | vm |
| `CRATONVM_DBG_MONEXIT` | 1 | yes | vm |
| `CRATONVM_DBG_MSC` | 1 | yes | native-builtins |
| `CRATONVM_DBG_MTROOTS` | 1 | yes | vm |
| `CRATONVM_DBG_NCDFE` | 1 | no | vm |
| `CRATONVM_DBG_NETTY_QUEUE` | 1 | yes | native-builtins |
| `CRATONVM_DBG_NOCODE` | 1 | no | vm |
| `CRATONVM_DBG_NO_CLEANERS` | 1 | yes | vm |
| `CRATONVM_DBG_NO_NONMOVING_RECLAIM` | 1 | no | gc |
| `CRATONVM_DBG_NO_PRUNE` | 1 | yes | vm |
| `CRATONVM_DBG_NO_REFPROC` | 1 | yes | vm |
| `CRATONVM_DBG_NPE_INVOKE` | 1 | no | vm |
| `CRATONVM_DBG_NPE_NONE` | 1 | no | vm |
| `CRATONVM_DBG_NPE_STACK` | 1 | no | vm |
| `CRATONVM_DBG_NPE_TRACE` | 1 | no | vm |
| `CRATONVM_DBG_NSME` | 1 | no | vm |
| `CRATONVM_DBG_NULL_NATIVE` | 1 | no | native-builtins |
| `CRATONVM_DBG_OBJ_EQUALS` | 1 | yes | native-builtins |
| `CRATONVM_DBG_OSR_META` | 1 | no | jit |
| `CRATONVM_DBG_OVERLAY` | 1 | no | vm |
| `CRATONVM_DBG_OVERLAY_ALL` | 1 | no | vm |
| `CRATONVM_DBG_PARKLAT` | 1 | yes | vm |
| `CRATONVM_DBG_PBE` | 1 | no | native-builtins |
| `CRATONVM_DBG_PBSTART` | 1 | no | vm |
| `CRATONVM_DBG_POPINT` | 1 | no | vm |
| `CRATONVM_DBG_PROXY` | 1 | no | native-builtins |
| `CRATONVM_DBG_RAF_INIT` | 1 | no | native-builtins |
| `CRATONVM_DBG_RE5` | 1 | no | native-builtins |
| `CRATONVM_DBG_REFERSTO` | 1 | yes | native-builtins |
| `CRATONVM_DBG_REFPROC_REMARK` | 1 | no | vm |
| `CRATONVM_DBG_REMAP_TRACE` | 1 | yes | vm |
| `CRATONVM_DBG_REPLOVR` | 1 | no | native-builtins |
| `CRATONVM_DBG_RESOLVE_SHIM` | 1 | no | native-builtins |
| `CRATONVM_DBG_RESOURCE_TIMING` | 1 | yes | classloading |
| `CRATONVM_DBG_RESUME_PC` | 1 | no | vm |
| `CRATONVM_DBG_ROOTSNAP` | 1 | yes | vm |
| `CRATONVM_DBG_RSET_AUDIT` | 1 | no | gc |
| `CRATONVM_DBG_RSET_AUDIT_YOUNG_SCAN` | 1 | no | gc |
| `CRATONVM_DBG_RVAS` | 1 | no | libcratonvm |
| `CRATONVM_DBG_SC_CLOSE` | 1 | no | native-io |
| `CRATONVM_DBG_SEED_ALL_OLD` | 1 | no | gc |
| `CRATONVM_DBG_SELECTOR` | 1 | yes | native-io |
| `CRATONVM_DBG_SHADOW2` | 1 | yes | jit |
| `CRATONVM_DBG_SHADOW2_FILTER` | 1 | no | jit |
| `CRATONVM_DBG_SHADOW_DEPTH` | 1 | no | vm |
| `CRATONVM_DBG_SHADOW_RELOAD` | 1 | yes | jit |
| `CRATONVM_DBG_SOE` | 1 | no | vm |
| `CRATONVM_DBG_STACKLESS` | 1 | no | vm |
| `CRATONVM_DBG_STALELONG` | 1 | yes | vm |
| `CRATONVM_DBG_STALE_OBJREF_CYCLES` | 1 | yes | gc |
| `CRATONVM_DBG_STALE_RECV` | 1 | no | vm |
| `CRATONVM_DBG_STREAMSUPP` | 1 | no | native-builtins |
| `CRATONVM_DBG_STW_NATIVE_RING` | 1 | no | vm |
| `CRATONVM_DBG_SWEEP_EDGES` | 1 | no | gc |
| `CRATONVM_DBG_SWEEP_ZERO` | 1 | yes | gc |
| `CRATONVM_DBG_THREADREG_PERF` | 1 | yes | vm |
| `CRATONVM_DBG_THREADSTART` | 1 | no | vm |
| `CRATONVM_DBG_TIER_ENQUEUE` | 1 | no | jit |
| `CRATONVM_DBG_TOHEX` | 1 | no | native-builtins |
| `CRATONVM_DBG_UCLREG` | 1 | no | native-builtins |
| `CRATONVM_DBG_UNCAUGHT` | 1 | no | vm |
| `CRATONVM_DBG_UNDERFLOW` | 1 | no | vm |
| `CRATONVM_DBG_UNPARK_MISS` | 1 | no | vm |
| `CRATONVM_DBG_UNPIN_RING` | 1 | yes | vm |
| `CRATONVM_DBG_URLCL` | 1 | no | native-builtins |
| `CRATONVM_DBG_UTE` | 1 | no | native-builtins |
| `CRATONVM_DBG_VALIDATE_NEW` | 1 | no | vm |
| `CRATONVM_DBG_VERIFY_ERROR` | 1 | no | vm |
| `CRATONVM_DBG_VERIFY_INLINE_FRAME_RECORD` | 1 | yes | jit |
| `CRATONVM_DBG_VERIFY_OOP_MAPS` | 1 | yes | vm |
| `CRATONVM_DBG_VISITFILE` | 1 | no | native-builtins |
| `CRATONVM_DBG_VM_STATE` | 1 | yes | vm |
| `CRATONVM_DBG_WATCHADDR` | 1 | yes | vm |
| `CRATONVM_DBG_WATCH_CAUSE_SELF` | 1 | no | native-builtins |
| `CRATONVM_DBG_WATCH_CELL` | 1 | yes | gc |
| `CRATONVM_DBG_WEAKREF` | 1 | yes | vm |
| `CRATONVM_DBG_WF_NPE` | 1 | no | vm |
| `CRATONVM_DBG_YOUNGSCAN` | 1 | yes | vm |
| `CRATONVM_DBG_ZERO_RANGES` | 1 | yes | gc |
| `CRATONVM_DEBUG_SFI` | 1 | no | native-builtins |
| `CRATONVM_DEBUG_STACK_TAG` | 1 | no | vm |
| `CRATONVM_DEOPT_VERIFY` | 1 | yes | jit |
| `CRATONVM_DIAG_HIB32` | 1 | no | gc |
| `CRATONVM_DIAG_JAR_LIST` | 1 | no | native-io |
| `CRATONVM_DIAG_JBOSS_SERVICES` | 1 | no | native-builtins |
| `CRATONVM_DIAG_JCA` | 1 | no | native-builtins |
| `CRATONVM_DIAG_METHOD_INVOKE_NULL` | 1 | no | native-builtins |
| `CRATONVM_DIAG_PROPERTIES` | 1 | no | native-builtins |
| `CRATONVM_ENABLE_ASSERTIONS` | 1 | no | native-builtins |
| `CRATONVM_EXEC_FRAME_TRACE` | 1 | no | vm |
| `CRATONVM_FORNAME_TRACE` | 1 | no | native-builtins |
| `CRATONVM_FRAME_TRACE` | 1 | no | vm |
| `CRATONVM_FWD_RESOLVE_STRICT` | 1 | yes | gc |
| `CRATONVM_G1_DBG_HEADERS` | 1 | no | gc |
| `CRATONVM_G1_DBG_PINS` | 1 | no | gc |
| `CRATONVM_G1_DBG_ROOTCENSUS` | 1 | no | gc |
| `CRATONVM_G1_DBG_ZERO` | 1 | no | gc |
| `CRATONVM_GC_ARRAY_GUARD_BT` | 1 | no | gc |
| `CRATONVM_GC_STATS` | 1 | no | vm-cli |
| `CRATONVM_GPU_TRACE_BYTES` | 1 | yes | vm |
| `CRATONVM_HM_TRACE` | 1 | yes | native-collections |
| `CRATONVM_HS_ITR_DBG` | 1 | yes | native-collections |
| `CRATONVM_IAE_TRACE2` | 1 | no | native-builtins |
| `CRATONVM_INTRINSIC_STATS` | 1 | no | vm-cli |
| `CRATONVM_INVOKESTATIC_LOADER_TRACE` | 1 | yes | vm |
| `CRATONVM_JBOSS_BOOT_LOG_FILE` | 1 | no | native-builtins |
| `CRATONVM_JBOSS_LOGGER_BASE_EMIT` | 1 | no | native-builtins |
| `CRATONVM_JIT_BISECT_ONLY` | 1 | yes | vm |
| `CRATONVM_JIT_BISECT_SKIP` | 1 | yes | vm |
| `CRATONVM_LDC_CLASSREF_TRACE` | 1 | no | vm |
| `CRATONVM_LOCK_ORDER_CHECK` | 1 | no | types |
| `CRATONVM_LONGROOT_STRICT` | 1 | yes | vm |
| `CRATONVM_MOVING_YOUNG_COVERAGE_DBG` | 1 | no | vm |
| `CRATONVM_MOVING_YOUNG_VERIFY` | 1 | yes | gc |
| `CRATONVM_NEEDS_EXACT_TRACE` | 1 | no | vm |
| `CRATONVM_NO_SELECTOR_CONNECT_PROBE` | 1 | yes | native-io |
| `CRATONVM_NSEE_TRACE` | 1 | no | vm |
| `CRATONVM_OOP_SPAN_PROBE` | 1 | no | types |
| `CRATONVM_QUICKEN_STATS` | 1 | yes | reader |
| `CRATONVM_S111_DBG` | 1 | yes | native-builtins |
| `CRATONVM_SHADOW_SENTINEL` | 1 | yes | jit |
| `CRATONVM_SHADOW_WATCH` | 1 | yes | jit |
| `CRATONVM_SOCKET_CAPTURE` | 1 | yes | native-io |
| `CRATONVM_SPRING_DBG` | 1 | yes | native-builtins |
| `CRATONVM_SP_STATS` | 1 | no | gc |
| `CRATONVM_SP_TRACE` | 1 | no | gc |
| `CRATONVM_SP_VERIFY` | 1 | no | gc |
| `CRATONVM_STRICT_SWALLOWS` | 1 | yes | vm |
| `CRATONVM_SUREFIRE_IPC_DBG` | 1 | yes | native-io |
| `CRATONVM_SYMBOLIZE` | 1 | no | vm-cli |
| `CRATONVM_SYMBOLIZE_DBG` | 1 | no | vm |
| `CRATONVM_TRACE_ARRAYS_HASHCODE` | 1 | no | native-builtins |
| `CRATONVM_TRACE_PTI_ARGS` | 1 | no | native-builtins |
| `CRATONVM_TRACE_SB_FILTER` | 1 | no | vm |
| `CRATONVM_TRACK_NATIVE` | 1 | yes | native-api |
| `CRATONVM_UEH_DEBUG` | 1 | no | native-builtins |

## 6. Class (c) — test-only flags

| Flag | Reads | Crates | Note |
| --- | ---: | --- | --- |
| `CRATONVM_BIN` | 86 | difftest,vm | vm/tests/sb_count_slot_guard_regression.rs:43 |
| `CRATONVM_DIFF_HOTSPOT` | 1 | vm | vm/tests/jit_interp_differential.rs:487 |
| `CRATONVM_JAVA_HOME` | 30 | native-builtins,vm | native-builtins/src/phases_late.rs:13581 |
| `CRATONVM_NONEXISTENT_VAR_12345` | 1 | vm | vm/src/vm.rs:11589 |
| `CRATONVM_REAL_RAF` | 2 | vm | vm/tests/synthetic_diff.rs:292 |
| `CRATONVM_REGEN_HEADER` | 1 | libcratonvm | libcratonvm/build.rs:35 |
| `CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS` | 1 | vm | vm/tests/interpreter_tests.rs:40 |
| `CRATONVM_SOAK_ITERS` | 1 | libcratonvm | libcratonvm/src/lib.rs:3256 |
| `CRATONVM_SOAK_K` | 1 | libcratonvm | libcratonvm/src/lib.rs:3255 |
| `CRATONVM_SOAK_METHOD` | 1 | libcratonvm | libcratonvm/src/lib.rs:3179 |
| `CRATONVM_SOAK_TIMEOUT_SECS` | 1 | libcratonvm | libcratonvm/src/lib.rs:3349 |
| `CRATONVM_SOAK_XMX` | 1 | libcratonvm | libcratonvm/src/lib.rs:3133 |
| `CRATONVM_SPRING_BOOT_FATJAR` | 1 | vm | vm/tests/wave3_spring_boot_fatjar.rs:167 |
| `CRATONVM_TEST_CLASSES_DIR` | 3 | vm | vm/tests/inet_socket_address_port_only.rs:68 |
| `CRATONVM_TEST_JAVA_HOME` | 11 | vm | vm/tests/wave3_b2_dispatch.rs:92 |
| `CRATONVM_TEST_JDK` | 13 | native-builtins,vm | native-builtins/src/phases_late.rs:13581 |
| `CRATONVM_TEST_SEGV` | 1 | vm-cli | vm-cli/src/main.rs:3559 |
| `CRATONVM_TEST_VAR` | 1 | vm | vm/src/vm.rs:11560 |

## 7. Scope boundary: `native-builtins/`

`native-builtins/` holds **366** read sites across **159** flag names, of which
**127** appear *only* there. None of them are touched by this branch: that crate is
concurrently being split from a single 86 000-line `lib.rs` into per-domain modules,
and editing it now would guarantee a destructive conflict. Migrating those sites is
a deliberate follow-up once the split lands. They are catalogued here so the
follow-up has the same evidence base.

| Flag | Reads in native-builtins | Class |
| --- | ---: | --- |
| `CRATONVM_ANN_TRACE` | 3 | a-diag |
| `CRATONVM_AOT_HMAC_KEY` | 1 | b-semantics |
| `CRATONVM_ASYNC_HANDOFF_SLEEP_FLOOR_MS` | 1 | b-semantics |
| `CRATONVM_ASYNC_SUBMIT_GRACE_MS` | 1 | b-semantics |
| `CRATONVM_ASYNC_WORKER_SLEEP_FLOOR_MS` | 1 | b-semantics |
| `CRATONVM_AWAIT_NO_SHORTCIRCUIT` | 1 | b-semantics |
| `CRATONVM_CANON_OPENFILE` | 1 | b-semantics |
| `CRATONVM_CL_BOOTSTRAP_SCOPED` | 1 | b-semantics |
| `CRATONVM_DBG` | 1 | a-diag |
| `CRATONVM_DBG_ANNPROXY_WRAP` | 1 | a-diag |
| `CRATONVM_DBG_AQS_TRACE` | 1 | a-diag |
| `CRATONVM_DBG_ARRAYCOPY` | 1 | a-diag |
| `CRATONVM_DBG_ASSERTJ_ARR` | 7 | a-diag |
| `CRATONVM_DBG_ATOMIC_UPDATER` | 1 | a-diag |
| `CRATONVM_DBG_BB` | 1 | a-diag |
| `CRATONVM_DBG_CALLER` | 1 | a-diag |
| `CRATONVM_DBG_CAPVAL` | 1 | a-diag |
| `CRATONVM_DBG_CAUSE` | 2 | a-diag |
| `CRATONVM_DBG_CCECACHE` | 1 | a-diag |
| `CRATONVM_DBG_CLONE` | 1 | a-diag |
| `CRATONVM_DBG_COERCE` | 1 | a-diag |
| `CRATONVM_DBG_COMPONENT_TYPE` | 1 | a-diag |
| `CRATONVM_DBG_DEFLATE` | 5 | a-diag |
| `CRATONVM_DBG_DOPRIV` | 1 | a-diag |
| `CRATONVM_DBG_EQE` | 2 | a-diag |
| `CRATONVM_DBG_FBREF` | 3 | a-diag |
| `CRATONVM_DBG_FIELD_GET` | 1 | a-diag |
| `CRATONVM_DBG_FSP` | 1 | a-diag |
| `CRATONVM_DBG_GOCBF` | 1 | a-diag |
| `CRATONVM_DBG_HTTPSRV` | 2 | a-diag |
| `CRATONVM_DBG_INVOKE_COERCE` | 1 | a-diag |
| `CRATONVM_DBG_ISINSTANCE` | 1 | a-diag |
| `CRATONVM_DBG_JLM` | 2 | a-diag |
| `CRATONVM_DBG_LAMBDA_GENERIC` | 4 | a-diag |
| `CRATONVM_DBG_LINKER` | 2 | a-diag |
| `CRATONVM_DBG_LOGPROV` | 1 | a-diag |
| `CRATONVM_DBG_LOOKUP` | 2 | a-diag |
| `CRATONVM_DBG_MCL` | 1 | a-diag |
| `CRATONVM_DBG_METHOD_INVOKE_BOX` | 1 | a-diag |
| `CRATONVM_DBG_MH_DISPATCH` | 6 | a-diag |
| `CRATONVM_DBG_MINVOKE` | 1 | a-diag |
| `CRATONVM_DBG_MSC` | 1 | a-diag |
| `CRATONVM_DBG_NETTY_QUEUE` | 1 | a-diag |
| `CRATONVM_DBG_NIO_BIND` | 6 | a-diag |
| `CRATONVM_DBG_NULL_NATIVE` | 1 | a-diag |
| `CRATONVM_DBG_OBJECTS` | 4 | a-diag |
| `CRATONVM_DBG_OBJ_EQUALS` | 1 | a-diag |
| `CRATONVM_DBG_PBE` | 1 | a-diag |
| `CRATONVM_DBG_PICOCLI_STYLE` | 2 | a-diag |
| `CRATONVM_DBG_PROXY` | 1 | a-diag |
| `CRATONVM_DBG_RAF_GETFD` | 2 | a-diag |
| `CRATONVM_DBG_RAF_INIT` | 1 | a-diag |
| `CRATONVM_DBG_RE5` | 1 | a-diag |
| `CRATONVM_DBG_REFERSTO` | 1 | a-diag |
| `CRATONVM_DBG_REPLOVR` | 1 | a-diag |
| `CRATONVM_DBG_RESOLVE_SHIM` | 1 | a-diag |
| `CRATONVM_DBG_SEL` | 4 | a-diag |
| `CRATONVM_DBG_SLEEP_TRACE` | 3 | a-diag |
| `CRATONVM_DBG_SOCK` | 3 | a-diag |
| `CRATONVM_DBG_SOCK_BYTES` | 2 | a-diag |
| `CRATONVM_DBG_STREAMSUPP` | 1 | a-diag |
| `CRATONVM_DBG_TLS_AUTH` | 41 | a-diag |
| `CRATONVM_DBG_TLS_HS` | 15 | a-diag |
| `CRATONVM_DBG_TLS_PLS` | 2 | a-diag |
| `CRATONVM_DBG_TLS_SOCK` | 14 | a-diag |
| `CRATONVM_DBG_TLS_SRV` | 3 | a-diag |
| `CRATONVM_DBG_TOHEX` | 1 | a-diag |
| `CRATONVM_DBG_UCLREG` | 1 | a-diag |
| `CRATONVM_DBG_UCLRES` | 3 | a-diag |
| `CRATONVM_DBG_URLCL` | 1 | a-diag |
| `CRATONVM_DBG_UTE` | 1 | a-diag |
| `CRATONVM_DBG_VISITFILE` | 1 | a-diag |
| `CRATONVM_DBG_WATCH_CAUSE_SELF` | 1 | a-diag |
| `CRATONVM_DBG_WF` | 4 | a-diag |
| `CRATONVM_DBG_XNIO_TCP` | 5 | a-diag |
| `CRATONVM_DEBUG_SFI` | 1 | a-diag |
| `CRATONVM_DEBUG_STACKWALK` | 5 | a-diag |
| `CRATONVM_DIAG_JBOSS_SERVICES` | 1 | a-diag |
| `CRATONVM_DIAG_JCA` | 1 | a-diag |
| `CRATONVM_DIAG_METHOD_INVOKE_NULL` | 1 | a-diag |
| `CRATONVM_DIAG_PROPERTIES` | 1 | a-diag |
| `CRATONVM_DIAG_SERVICELOADER` | 5 | a-diag |
| `CRATONVM_ENABLE_ASSERTIONS` | 1 | a-diag |
| `CRATONVM_EQE_SYNC_EXECUTE` | 1 | b-semantics |
| `CRATONVM_FORNAME_TRACE` | 1 | a-diag |
| `CRATONVM_HTTP_MAX_BODY` | 1 | b-semantics |
| `CRATONVM_IAE_TRACE2` | 1 | a-diag |
| `CRATONVM_INHERIT_THREAD_CCL` | 1 | b-semantics |
| `CRATONVM_INHERIT_TL_WORKAROUND` | 1 | b-semantics |
| `CRATONVM_JBOSS_BOOT_LOG_FILE` | 1 | a-diag |
| `CRATONVM_JBOSS_BRUTE_FORCE_JARS` | 1 | b-semantics |
| `CRATONVM_JBOSS_LOGGER_BASE_EMIT` | 1 | a-diag |
| `CRATONVM_JBOSS_MP_ROOT` | 8 | b-semantics |
| `CRATONVM_MAVEN_REPO_LOCAL` | 2 | b-semantics |
| `CRATONVM_MAX_INFLATED_BYTES` | 1 | b-semantics |
| `CRATONVM_MSC_REAL_START` | 1 | b-semantics |
| `CRATONVM_NATIVE_EC_MULTIPLY` | 1 | b-semantics |
| `CRATONVM_NATIVE_PBE_KEYFACTORY` | 1 | b-semantics |
| `CRATONVM_NETTY_QUEUE_BRIDGE` | 1 | b-semantics |
| `CRATONVM_REAL_AGROAL` | 1 | b-semantics |
| `CRATONVM_REAL_PROXY` | 1 | b-semantics |
| `CRATONVM_REAL_PROXY_STRICT` | 1 | b-semantics |
| `CRATONVM_REAL_QUARKUS_START` | 1 | b-semantics |
| `CRATONVM_REAL_STAX_FACTORY` | 1 | b-semantics |
| `CRATONVM_REAL_VERTX` | 1 | b-semantics |
| `CRATONVM_REQUIRE_POLICY` | 1 | b-semantics |
| `CRATONVM_S111_DBG` | 1 | a-diag |
| `CRATONVM_SFI_NULL_TRACE` | 2 | a-diag |
| `CRATONVM_SOFT_EXIT` | 5 | b-semantics |
| `CRATONVM_SPRING_DBG` | 1 | a-diag |
| `CRATONVM_SYNTHETIC_AGROAL` | 1 | b-semantics |
| `CRATONVM_SYNTHETIC_ANNOTATIONS` | 1 | b-semantics |
| `CRATONVM_SYNTHETIC_AQS` | 5 | b-semantics |
| `CRATONVM_SYNTHETIC_BUFFERED_WRITER` | 1 | b-semantics |
| `CRATONVM_SYNTHETIC_DSA` | 1 | b-semantics |
| `CRATONVM_SYNTHETIC_EC` | 1 | b-semantics |
| `CRATONVM_SYNTHETIC_EQE` | 1 | b-semantics |
| `CRATONVM_SYNTHETIC_PQC` | 1 | b-semantics |
| `CRATONVM_SYNTHETIC_QUARKUS_ARC` | 2 | b-semantics |
| `CRATONVM_SYNTHETIC_RSA` | 1 | b-semantics |
| `CRATONVM_SYNTHETIC_VERTX` | 1 | b-semantics |
| `CRATONVM_TRACE_ARRAYS_HASHCODE` | 1 | a-diag |
| `CRATONVM_TRACE_PTI_ARGS` | 1 | a-diag |
| `CRATONVM_UEH_DEBUG` | 1 | a-diag |
| `CRATONVM_URI_STRICT_CHARS` | 2 | b-semantics |
| `CRATONVM_USE_WILDFLY_REFLECT_SHIM` | 1 | b-semantics |
| `CRATONVM_USE_WILDFLY_SYNTH_BYTECODE` | 2 | b-semantics |

## 8. Caching status and the per-call readers

`std::env::var` takes a process-global lock in libc `getenv` and allocates. Of the
1145 read sites, **911** are not behind a `OnceLock`. Most of those are cold
(startup, class load, JIT compile), but three are genuinely hot and are the reason
the typed config is worth doing on performance grounds alone:

| Site | Frequency | Note |
| --- | --- | --- |
| `native-builtins/src/lang_system.rs:752` `CRATONVM_INHERIT_THREAD_CCL` | once per `Thread.start0` | out of scope this branch |
| `jit/src/x64.rs:2231` `CRATONVM_JIT_GETFIELD_HELPER` | once per compiled `getfield` **call site** | deliberately uncached — see the comment at `x64.rs:2225`, which argues caching would make the off-switch racy against whichever thread first triggers a getfield compile. Preserved as-is. |
| `gc/src/gen_heap.rs:3702` `CRATONVM_NO_GC_PROMOTION_GUARD` | per promotion-OOM check | per young-GC, not per object |

The `x64.rs` case is the interesting one: it is *deliberately* uncached, and the
reason given is that caching changes when the value is latched. A typed config
latches at startup, which is strictly earlier and therefore not racy — but it does
mean a test that flips the var mid-process stops working. That trade is called out
in the migration notes rather than made silently.

## 9. Existing precedents in the tree

The refactor extends what is already there rather than inventing a parallel system:

* `vm/src/runtime/env_cache.rs` (933 lines) — the partial precedent. `cached_is_set!`
  / `cached_is_ok!` macros wrap ~100 flags in per-flag `OnceLock`s. Right idea, but
  it lives in `vm`, which `types`/`gc`/`jit`/`classloading` cannot depend on, so
  those crates all grew their own copies.
* `jit/src/tiered.rs:199` `TieredParams::from_env()` / `with_overrides()` — already a
  typed struct with an injectable source. This is the shape the whole config should
  have, and `with_overrides` is what makes it unit-testable without touching process
  env. Adopted directly.
* `native-io/src/lib.rs:168` `env_flag_enabled()` — a third, independent boolean
  parser with its own `0`/`false`/`off`/`no` truth table. There are at least four
  such truth tables in the tree and they do **not** agree (see §10).
* `types/src/lock_order.rs` — the precedent for putting a cross-crate concept in
  `cratonvm-types`, the crate every other crate already depends on.

## 10. Finding: four disagreeing boolean truth tables

There is no single answer to "what does `CRATONVM_FOO=false` mean". The tree
contains at least these four:

| Parser | `unset` | `""` | `"0"` | `"false"` | `"off"` | `"no"` | anything else |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `var_os(..).is_some()` (the ~600-site majority) | false | **true** | **true** | true | true | true | true |
| `env_cache::disable_jit` (`env_cache.rs:70`) | false | **false** | **false** | true | true | true | true |
| `native_io::env_flag_enabled` (`lib.rs:168`) | false | false | false | **false** | **false** | **false** | true |
| `TieredParams` `tiered_enabled` (`tiered.rs:223`) | true | true | **false** | **false** | true | true | true |

So `CRATONVM_X=0` *enables* the feature at ~600 sites and *disables* it at three
others. This is a genuine footgun and the single strongest argument for one typed
config: the parse happens once, in one place, with one documented truth table.

**This branch does not unify the truth tables.** Each migrated flag keeps its own
parse function byte-for-byte, because changing `X=0` from "on" to "off" for 600
flags is a behaviour change, not a plumbing change. The typed config makes the
divergence *visible* (each field records which parser it uses) so it can be
retired deliberately, flag by flag, with benchmarks.

## 11. Reproducing this census

```sh
python3 tools/flag-census/census.py            # totals, from the repo root
python3 tools/flag-census/census.py /path/to/repo
```

The scan is deliberately literal-only: there is **no** dynamic env-var name
construction anywhere in the workspace (verified: no `format!("CRATONVM_{}", ..)`
and no `env::var(<non-literal>)` outside test JDK-discovery helpers and the three
injectable helpers named in §9), so a literal scan is exhaustive. `census.py`
re-checks that invariant on every run and aborts if it is ever violated.

