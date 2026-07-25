# CratonVM `CRATONVM_*` environment-flag census

*Generated 2026-07-25 from `dev` @ `531f5f710` by a mechanical scan of every
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
| Distinct `CRATONVM_*` identifiers seen anywhere (code, docs, scripts) | **692** |
| …of which have at least one Rust read site | **559** |
| Rust code literal sites (all kinds) | **1009** |
| Rust *read* sites (excludes `set_var`/`env_remove`/`option_env!`) | **928** |
| Read sites **outside** `native-builtins/` (this refactor's scope) | **913** |
| Read sites inside `native-builtins/` (deliberately deferred, see §6) | **15** |
| …of which still call `std::env::var` / `var_os` directly | **641** |
| …already reading a `VmFlags` field instead | **287** |
| Remaining direct read sites that are **not** `OnceLock`-cached | **464** |
| In-process `set_var` / `remove_var` / `Command::env` sites | **72** |

### Classification

| Class | Meaning | Count |
| --- | --- | ---: |
| **(a) debug / diagnostic** | only gates `eprintln!`/tracing/extra verification; removing it cannot change a program's result | 340 |
| **(b) semantics-changing** | selects a different code path, algorithm, layout or default; two settings are two different VMs | 201 |
| **(c) test-only** | read only from `tests/`, `benches/`, `build.rs` or a soak/difftest harness | 18 |
| **(d) dead** | **no Rust read site at all** — referenced only by docs, scripts or comments | 133 |
| | | **692** |

The (b) count is the headline number. 2^201 is not a testable behaviour space, and
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
| `CRATONVM_ALLOW_JSR_RET` | 2 | **no** | value/other | classloading,types | `classloading/src/verifier.rs:2842` |
| `CRATONVM_ALLOW_MOVING_YOUNG` | 1 | **no** | value/other | types | `types/src/flags.rs:600` |
| `CRATONVM_AOT_HMAC_KEY` | 1 | **no** | value/other | native-builtins | `native-builtins/src/aot.rs:265` |
| `CRATONVM_ASYNC_HANDOFF_SLEEP_FLOOR_MS` | 1 | **no** | value/other | types | `types/src/flags.rs:1391` |
| `CRATONVM_ASYNC_SUBMIT_GRACE_MS` | 1 | **no** | value/other | types | `types/src/flags.rs:1392` |
| `CRATONVM_ASYNC_WORKER_SLEEP_FLOOR_MS` | 1 | **no** | value/other | types | `types/src/flags.rs:1393` |
| `CRATONVM_AWAIT_NO_SHORTCIRCUIT` | 1 | **no** | value/other | types | `types/src/flags.rs:1394` |
| `CRATONVM_BG_COMPILE` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:464` |
| `CRATONVM_BLOCK_PRIVATE_NETS` | 1 | **no** | value/other | types | `types/src/flags.rs:858` |
| `CRATONVM_BOOT_MODULE_REGISTRY` | 2 | **no** | value/other | types | `types/src/flags.rs:759` |
| `CRATONVM_C2_SUPERSEDE` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:832` |
| `CRATONVM_CANON_OPENFILE` | 1 | **no** | value/other | types | `types/src/flags.rs:1396` |
| `CRATONVM_CARD_TABLE_ONLY` | 2 | **no** | value/other | types | `types/src/flags.rs:606` |
| `CRATONVM_CL_BOOTSTRAP_SCOPED` | 1 | **no** | value/other | types | `types/src/flags.rs:1397` |
| `CRATONVM_COMPACT_REF_FIELDS` | 1 | yes | value/other | types | `types/src/field_layout.rs:175` |
| `CRATONVM_COMPRESSED_OOPS` | 1 | **no** | value/other | vm | `vm/src/vm/vm_init.rs:859` |
| `CRATONVM_CONFINE_IO` | 2 | **no** | value/other | types | `types/src/flags.rs:856` |
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
| `CRATONVM_DISABLE_JAR_MMAP` | 1 | **no** | value/other | types | `types/src/flags.rs:765` |
| `CRATONVM_DISABLE_JIT` | 5 | partial | value/other | difftest,vm | `vm/tests/wave2_bc_probe.rs:296` |
| `CRATONVM_DISABLE_SCALAR_REPLACEMENT` | 1 | **no** | opt-in (default OFF) | jit | `jit/src/x64.rs:28365` |
| `CRATONVM_DISABLE_UNROLL` | 1 | **no** | opt-in (default OFF) | jit | `jit/src/x64.rs:28214` |
| `CRATONVM_EAGER_STREAMS` | 1 | yes | opt-out (default ON) | native-collections | `native-collections/src/lib.rs:13160` |
| `CRATONVM_ENABLE_NATIVE_RING` | 1 | **no** | value/other | vm-cli | `vm-cli/src/main.rs:2220` |
| `CRATONVM_EQE_SYNC_EXECUTE` | 1 | **no** | value/other | types | `types/src/flags.rs:1488` |
| `CRATONVM_EXEC_DEPTH_CEILING` | 1 | **no** | value/other | vm | `vm/src/runtime/interpreter.rs:4823` |
| `CRATONVM_FORCE_WIN_BUILD` | 1 | **no** | value/other | vm | `vm/src/vm/vm_init.rs:180` |
| `CRATONVM_FOREIGN_ATTACH` | 1 | **no** | value/other | vm | `vm/src/native/jni.rs:761` |
| `CRATONVM_FUZZ_BOOTCP` | 1 | **no** | value/other | fuzz | `fuzz/fuzz_targets/fuzz_verifier.rs:55` |
| `CRATONVM_G1_NO_EVAC_RETRY` | 1 | **no** | value/other | types | `types/src/flags.rs:609` |
| `CRATONVM_G1_PARALLEL_EVAC` | 5 | **no** | value/other | types | `types/src/flags.rs:608` |
| `CRATONVM_G1_WORKERS` | 4 | **no** | value/other | types | `types/src/flags.rs:610` |
| `CRATONVM_GC_OVERHEAD_LIMIT` | 1 | yes | value/other | vm | `vm/src/runtime/interpreter.rs:1637` |
| `CRATONVM_GC_PAR_MIN_BYTES` | 1 | yes | value/other | gc | `gc/src/young_mark.rs:193` |
| `CRATONVM_GC_PAR_THREADS` | 1 | yes | value/other | gc | `gc/src/young_mark.rs:170` |
| `CRATONVM_GC_STRESS` | 4 | **no** | value/other | types | `types/src/flags.rs:612` |
| `CRATONVM_GC_SWEEP_ANCHOR_STRIDE` | 1 | yes | value/other | gc | `gc/src/gen_heap.rs:99` |
| `CRATONVM_GPU_NO_ZEROCOPY` | 1 | yes | opt-out (default ON) | vm | `vm/src/runtime/gpu_marshal.rs:689` |
| `CRATONVM_HARDEN_MANIFEST_CLASSPATH` | 1 | **no** | value/other | types | `types/src/flags.rs:763` |
| `CRATONVM_HELPFUL_NPE_OPCODES` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:326` |
| `CRATONVM_HTTP_MAX_BODY` | 1 | **no** | value/other | types | `types/src/flags.rs:1490` |
| `CRATONVM_INHERIT_THREAD_CCL` | 1 | **no** | value/other | types | `types/src/flags.rs:1494` |
| `CRATONVM_INHERIT_TL_WORKAROUND` | 1 | **no** | value/other | types | `types/src/flags.rs:1495` |
| `CRATONVM_INLINE_ALLOW_STATIC` | 1 | yes | opt-in (default OFF) | vm | `vm/src/runtime/env_cache.rs:869` |
| `CRATONVM_IR_DEOPT_RESUME` | 1 | yes | opt-in (default OFF) | vm | `vm/src/runtime/interpreter.rs:11621` |
| `CRATONVM_JBOSS_BRUTE_FORCE_JARS` | 1 | **no** | value/other | types | `types/src/flags.rs:1497` |
| `CRATONVM_JBOSS_MP_ROOT` | 8 | **no** | value/other | native-builtins | `native-builtins/src/lib.rs:71` |
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
| `CRATONVM_JIT_IR_CALL` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:839` |
| `CRATONVM_JIT_IR_CALL_SPECIAL` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:845` |
| `CRATONVM_JIT_IR_CALL_VIRTUAL` | 1 | yes | opt-in (default OFF) | vm | `vm/src/runtime/env_cache.rs:857` |
| `CRATONVM_JIT_IR_DIRECT_CALL` | 1 | **no** | value/other | jit | `jit/src/lib.rs:5819` |
| `CRATONVM_JIT_IR_FP` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:863` |
| `CRATONVM_JIT_IR_LONG` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:851` |
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
| `CRATONVM_JIT_SCALAR_NEW` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:819` |
| `CRATONVM_JIT_STACK_BANG` | 1 | yes | value/other | jit | `jit/src/x64.rs:183` |
| `CRATONVM_JIT_THRESHOLD` | 3 | partial | value/other | difftest,vm | `vm/src/runtime/env_cache.rs:88` |
| `CRATONVM_JIT_UNBAN_JUNITCORE` | 1 | **no** | opt-in (default OFF) | vm | `vm/src/jit/skip_list.rs:915` |
| `CRATONVM_JIT_UNROLL` | 1 | yes | value/other | jit | `jit/src/ir_optimize.rs:1905` |
| `CRATONVM_JIT_VIRTUAL_TIERUP` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:494` |
| `CRATONVM_LAZY_STREAMS` | 1 | yes | opt-in (default OFF) | native-collections | `native-collections/src/lib.rs:13157` |
| `CRATONVM_LENIENT_CLINIT` | 2 | partial | value/other | vm | `vm/src/vm/vm_util.rs:53` |
| `CRATONVM_LOADER_AWARE_RESOLUTION` | 2 | **no** | value/other | types | `types/src/flags.rs:757` |
| `CRATONVM_LOADER_UNLOAD` | 2 | partial | value/other | types | `types/src/flags.rs:1499` |
| `CRATONVM_LONGREWRITE_LOOSE` | 1 | yes | opt-in (default OFF) | vm | `vm/src/runtime/value_stack.rs:198` |
| `CRATONVM_MAVEN_REPO_LOCAL` | 2 | **no** | value/other | native-builtins | `native-builtins/src/jboss_module_loader.rs:301` |
| `CRATONVM_MAX_INFLATED_BYTES` | 1 | **no** | value/other | types | `types/src/flags.rs:1500` |
| `CRATONVM_MOVING_YOUNG` | 5 | partial | opt-in (default OFF) | jit,types,vm | `jit/src/x64.rs:2458` |
| `CRATONVM_MOVING_YOUNG_FALLBACKS` | 1 | **no** | value/other | types | `types/src/flags.rs:601` |
| `CRATONVM_MSC_REAL_START` | 1 | **no** | value/other | types | `types/src/flags.rs:1501` |
| `CRATONVM_NATIVE_EC_MULTIPLY` | 1 | **no** | value/other | types | `types/src/flags.rs:1502` |
| `CRATONVM_NATIVE_MATCHER_FIND` | 2 | partial | value/other | types,vm | `vm/src/runtime/env_cache.rs:562` |
| `CRATONVM_NATIVE_PBE_KEYFACTORY` | 1 | **no** | value/other | types | `types/src/flags.rs:1504` |
| `CRATONVM_NATIVE_STRING_REGEX` | 2 | partial | value/other | types,vm | `vm/src/runtime/env_cache.rs:520` |
| `CRATONVM_NETTY_QUEUE_BRIDGE` | 1 | **no** | value/other | types | `types/src/flags.rs:1506` |
| `CRATONVM_NO_CONSERVATIVE_LOCALS` | 1 | yes | opt-out (default ON) | vm | `vm/src/memory/roots.rs:36` |
| `CRATONVM_NO_CTOR_DIRECT_CALL` | 1 | **no** | value/other | vm | `vm/src/runtime/env_cache.rs:587` |
| `CRATONVM_NO_GC_PROMOTION_GUARD` | 1 | **no** | value/other | types | `types/src/flags.rs:602` |
| `CRATONVM_NO_IR_BRANCHY` | 1 | yes | opt-out (default ON) | jit | `jit/src/ir_optimize.rs:98` |
| `CRATONVM_NO_JIT_ALLOC_CLASS_CACHE` | 1 | yes | opt-out (default ON) | vm | `vm/src/jit/alloc_class_cache.rs:218` |
| `CRATONVM_NO_JIT_INLINE_PUTFIELD` | 1 | yes | opt-out (default ON) | jit | `jit/src/x64.rs:2126` |
| `CRATONVM_NO_JIT_INLINE_TLAB_NEW` | 1 | yes | opt-out (default ON) | jit | `jit/src/x64.rs:2149` |
| `CRATONVM_NO_JIT_SCAN_CACHE` | 1 | **no** | opt-out (default ON) | vm | `vm/src/jit/conservative_roots.rs:899` |
| `CRATONVM_NO_LOCAL_LIVENESS` | 1 | **no** | value/other | vm | `vm/src/runtime/env_cache.rs:719` |
| `CRATONVM_NO_PRECISE_INLINE_FRAME_RECORD` | 1 | yes | opt-out (default ON) | jit | `jit/src/x64.rs:2288` |
| `CRATONVM_NO_PRECISE_JIT_MAPS` | 1 | yes | opt-out (default ON) | jit | `jit/src/x64.rs:2085` |
| `CRATONVM_NO_PRECISE_REG_SPILL` | 1 | yes | opt-in (default OFF) | jit | `jit/src/x64.rs:2661` |
| `CRATONVM_NO_SELECTIVE_PROMOTE` | 3 | **no** | value/other | difftest,types | `difftest/src/runner.rs:179` |
| `CRATONVM_NO_STUBS` | 1 | **no** | value/other | native-api | `native-api/src/registry.rs:3886` |
| `CRATONVM_OLD_SWEEP_JIT` | 4 | **no** | value/other | types | `types/src/flags.rs:607` |
| `CRATONVM_OSR_EXIT_AFTER` | 1 | yes | value/other | jit | `jit/src/lib.rs:1062` |
| `CRATONVM_OSR_EXIT_TEST` | 1 | yes | opt-in (default OFF) | jit | `jit/src/lib.rs:1042` |
| `CRATONVM_OSR_NEWARRAY` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:266` |
| `CRATONVM_PRECISE_COVERAGE_PIN` | 1 | yes | opt-in (default OFF) | vm | `vm/src/jit/conservative_roots.rs:1913` |
| `CRATONVM_PROMOTION_OOM_GUARD_BROAD` | 1 | **no** | value/other | types | `types/src/flags.rs:603` |
| `CRATONVM_REAL` | 2 | partial | value/other | vm | `vm/tests/synthetic_diff.rs:289` |
| `CRATONVM_REAL_AGROAL` | 1 | **no** | value/other | types | `types/src/flags.rs:1507` |
| `CRATONVM_REAL_ANNOTATIONS` | 3 | **no** | value/other | types,vm | `vm/tests/synthetic_diff.rs:290` |
| `CRATONVM_REAL_AQS` | 3 | **no** | value/other | types,vm | `vm/tests/synthetic_diff.rs:291` |
| `CRATONVM_REAL_FORKJOINPOOL` | 6 | partial | opt-out (default ON) | native-api,vm | `vm/tests/synthetic_diff.rs:294` |
| `CRATONVM_REAL_JCA` | 2 | partial | value/other | types,vm | `vm/src/runtime/env_cache.rs:966` |
| `CRATONVM_REAL_NET_SOCKETS` | 4 | partial | opt-in (default OFF) | native-api,types,vm | `vm/tests/synthetic_diff.rs:293` |
| `CRATONVM_REAL_PROXY` | 1 | **no** | value/other | types | `types/src/flags.rs:1511` |
| `CRATONVM_REAL_PROXY_STRICT` | 1 | **no** | value/other | types | `types/src/flags.rs:1512` |
| `CRATONVM_REAL_PROXY_SUPER` | 3 | partial | value/other | types,vm | `vm/src/runtime/env_cache.rs:354` |
| `CRATONVM_REAL_QUARKUS_START` | 1 | **no** | value/other | types | `types/src/flags.rs:1515` |
| `CRATONVM_REAL_STAX_FACTORY` | 1 | **no** | value/other | types | `types/src/flags.rs:1516` |
| `CRATONVM_REAL_VERTX` | 1 | **no** | value/other | types | `types/src/flags.rs:1517` |
| `CRATONVM_RECLAIM_DEAD_MONITORS` | 1 | yes | opt-in (default OFF) | vm | `vm/src/threading/monitor.rs:95` |
| `CRATONVM_REQUIRE_POLICY` | 1 | **no** | value/other | types | `types/src/flags.rs:1518` |
| `CRATONVM_RESOLVE_CACHE_CAP` | 4 | **no** | value/other | vm | `vm/src/runtime/lockfree_resolve.rs:56` |
| `CRATONVM_RESOLVE_OUTBOUND_HOST` | 1 | **no** | value/other | types | `types/src/flags.rs:859` |
| `CRATONVM_ROOTSNAP_CACHE` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:389` |
| `CRATONVM_ROOTSNAP_CACHE_SURVIVE_GC` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:422` |
| `CRATONVM_SCALAR_DEOPT` | 1 | yes | opt-in (default OFF) | jit | `jit/src/lib.rs:987` |
| `CRATONVM_SELECT_MAX_BLOCK_MS` | 3 | **no** | value/other | types | `types/src/flags.rs:864` |
| `CRATONVM_SHADOW_NOPUSH` | 2 | partial | opt-in (default OFF) | jit,vm | `jit/src/x64.rs:2501` |
| `CRATONVM_SHADOW_NORELOAD` | 2 | partial | opt-in (default OFF) | jit,vm | `jit/src/x64.rs:2511` |
| `CRATONVM_SHADOW_NO_SAVEBASE` | 1 | yes | opt-in (default OFF) | jit | `jit/src/x64.rs:2583` |
| `CRATONVM_SHADOW_PIN` | 2 | yes | opt-in (default OFF) | jit,vm | `jit/src/x64.rs:2525` |
| `CRATONVM_SHADOW_RAW_RELOAD` | 1 | yes | opt-in (default OFF) | jit | `jit/src/x64.rs:2572` |
| `CRATONVM_SHADOW_STACK` | 4 | partial | opt-in (default OFF) | jit,types,vm | `jit/src/x64.rs:2436` |
| `CRATONVM_SOFT_EXIT` | 1 | **no** | value/other | types | `types/src/flags.rs:1521` |
| `CRATONVM_SP_NO_COALESCE` | 1 | **no** | value/other | types | `types/src/flags.rs:605` |
| `CRATONVM_STRICT_JIT_ROOTS` | 1 | yes | opt-in (default OFF) | vm | `vm/src/jit/conservative_roots.rs:1223` |
| `CRATONVM_SYNTHETIC_AGROAL` | 1 | **no** | value/other | types | `types/src/flags.rs:1523` |
| `CRATONVM_SYNTHETIC_ANNOTATIONS` | 1 | **no** | value/other | types | `types/src/flags.rs:1524` |
| `CRATONVM_SYNTHETIC_AQS` | 1 | **no** | value/other | types | `types/src/flags.rs:1525` |
| `CRATONVM_SYNTHETIC_BUFFERED_WRITER` | 1 | **no** | value/other | types | `types/src/flags.rs:1526` |
| `CRATONVM_SYNTHETIC_DSA` | 1 | **no** | value/other | types | `types/src/flags.rs:1527` |
| `CRATONVM_SYNTHETIC_EC` | 1 | **no** | value/other | types | `types/src/flags.rs:1528` |
| `CRATONVM_SYNTHETIC_EQE` | 1 | **no** | value/other | types | `types/src/flags.rs:1529` |
| `CRATONVM_SYNTHETIC_FILEWRITER` | 1 | **no** | value/other | types | `types/src/flags.rs:861` |
| `CRATONVM_SYNTHETIC_PQC` | 1 | **no** | value/other | types | `types/src/flags.rs:1530` |
| `CRATONVM_SYNTHETIC_QUARKUS_ARC` | 2 | **no** | value/other | native-builtins | `native-builtins/src/quarkus_arc.rs:374` |
| `CRATONVM_SYNTHETIC_RAF` | 4 | **no** | value/other | types | `types/src/flags.rs:862` |
| `CRATONVM_SYNTHETIC_RSA` | 1 | **no** | value/other | types | `types/src/flags.rs:1531` |
| `CRATONVM_SYNTHETIC_VERTX` | 1 | **no** | value/other | types | `types/src/flags.rs:1532` |
| `CRATONVM_THREAD_START_GRACE_MS` | 1 | yes | value/other | vm | `vm/src/vm/vm_exec.rs:139` |
| `CRATONVM_TIER_C1_THRESHOLD` | 2 | **no** | value/other | jit | `jit/src/tiered.rs:211` |
| `CRATONVM_TIER_C2_MIN_INVOCATIONS` | 2 | **no** | value/other | jit | `jit/src/tiered.rs:220` |
| `CRATONVM_TIER_C2_THRESHOLD` | 2 | **no** | value/other | jit | `jit/src/tiered.rs:214` |
| `CRATONVM_TIER_ENABLED` | 2 | **no** | value/other | jit | `jit/src/tiered.rs:223` |
| `CRATONVM_TIER_OSR_BACKEDGE` | 1 | yes | value/other | vm | `vm/src/runtime/env_cache.rs:236` |
| `CRATONVM_TIER_OSR_THRESHOLD` | 2 | **no** | value/other | jit | `jit/src/tiered.rs:217` |
| `CRATONVM_TIER_PGO` | 1 | **no** | value/other | vm | `vm/src/runtime/env_cache.rs:479` |
| `CRATONVM_TLAB_GC_TRIGGER` | 1 | yes | value/other | vm | `vm/src/runtime/interpreter.rs:2692` |
| `CRATONVM_TRUST_PEM` | 2 | **no** | value/other | classloading,types | `classloading/src/jar_signer.rs:1941` |
| `CRATONVM_UNTRUSTED_CODE` | 1 | **no** | value/other | types | `types/src/flags.rs:857` |
| `CRATONVM_URI_STRICT_CHARS` | 1 | **no** | value/other | types | `types/src/flags.rs:1537` |
| `CRATONVM_USE_WILDFLY_REFLECT_SHIM` | 1 | **no** | value/other | types | `types/src/flags.rs:1538` |
| `CRATONVM_USE_WILDFLY_SYNTH_BYTECODE` | 1 | **no** | value/other | types | `types/src/flags.rs:1539` |
| `CRATONVM_WEAKREF_CLEAR` | 1 | yes | value/other | vm | `vm/src/runtime/interpreter.rs:148` |
| `CRATONVM_XT_HELPER_WINDOW_SCAN` | 1 | **no** | value/other | vm | `vm/src/jit/xt_root_scan.rs:127` |
| `CRATONVM_XT_JIT_ROOT_SCAN` | 2 | partial | value/other | jit,vm | `jit/src/lib.rs:879` |
| `CRATONVM_YOUNGSCAN_STRIDE` | 1 | yes | value/other | vm | `vm/src/vm/vm_exec.rs:668` |
| `CRATONVM_ZIP_MAX_ENTRY_BYTES` | 2 | **no** | value/other | types | `types/src/flags.rs:869` |

## 5. Class (a) — debug / diagnostic flags

340 flags. These gate `eprintln!` / `tracing` output or extra assertions
only. They are the bulk of the surface and the best candidates for collapsing
behind a single `experimental-diag` cargo feature: with the feature off the
accessor becomes `const false` and the whole diagnostic block folds away.

| Flag | Reads | Cached | Crates |
| --- | ---: | :---: | --- |
| `CRATONVM_DBG_DEOPT` | 17 | no | jit,vm |
| `CRATONVM_DBG_BLOCKGC` | 8 | partial | vm |
| `CRATONVM_DBG_SCALAR_DEOPT` | 8 | no | jit,vm |
| `CRATONVM_DBG_ARGS` | 7 | no | vm-cli |
| `CRATONVM_DBG_TOARRAY` | 6 | partial | native-collections,types,vm |
| `CRATONVM_DBG_BUG03` | 5 | no | vm |
| `CRATONVM_IAE_TRACE` | 5 | partial | types,vm |
| `CRATONVM_DBG_BLOCKED_ACCESS` | 4 | no | types |
| `CRATONVM_DBG_FIELD_WATCH` | 4 | partial | types,vm |
| `CRATONVM_DBG_JITC` | 4 | no | jit,vm |
| `CRATONVM_DBG_JIT_GEN` | 4 | no | jit |
| `CRATONVM_DBG_MIRRORPIN` | 4 | no | native-collections,types,vm |
| `CRATONVM_DBG_RBC6` | 4 | partial | jit,vm |
| `CRATONVM_DBG_RETRANSFORM` | 4 | no | vm |
| `CRATONVM_DBG_STALE_OBJREF_CYCLES` | 4 | no | types |
| `CRATONVM_DBG_STTRACE` | 4 | no | types,vm |
| `CRATONVM_DBG_STW_CENSUS` | 4 | no | vm |
| `CRATONVM_DBG_WATCH_CELL` | 4 | no | types |
| `CRATONVM_DBG_ALTRACE` | 3 | partial | native-collections,vm |
| `CRATONVM_DBG_CCE_BT` | 3 | partial | native-collections,types,vm |
| `CRATONVM_DBG_COMPACT_INLINE` | 3 | no | jit |
| `CRATONVM_DBG_NULLTHIS` | 3 | no | vm |
| `CRATONVM_DBG_OOBFIELD` | 3 | no | types |
| `CRATONVM_DBG_PRECISE` | 3 | no | types,vm |
| `CRATONVM_DBG_REFLECTION_FACTORY` | 3 | no | types,vm |
| `CRATONVM_DBG_STRAYSTACK` | 3 | partial | vm |
| `CRATONVM_DBG_TLABMISS` | 3 | yes | vm |
| `CRATONVM_DBG_UNROLL` | 3 | no | jit |
| `CRATONVM_DBG_WATCHREF` | 3 | no | types,vm |
| `CRATONVM_DBG_XT_JIT_ROOT_SCAN` | 3 | no | vm |
| `CRATONVM_SOCKET_CAPTURE` | 3 | no | types |
| `CRATONVM_TRACE_CLASSVALUE` | 3 | partial | types,vm |
| `CRATONVM_BD_DEBUG` | 2 | no | types,vm |
| `CRATONVM_DBG_A2` | 2 | no | types,vm |
| `CRATONVM_DBG_AIOOBE` | 2 | partial | vm |
| `CRATONVM_DBG_CATALINA` | 2 | no | types,vm |
| `CRATONVM_DBG_CORRUPT_FRAMES` | 2 | no | vm |
| `CRATONVM_DBG_CTOR_FIX` | 2 | no | vm |
| `CRATONVM_DBG_EXEC` | 2 | no | native-collections,types |
| `CRATONVM_DBG_EXIT` | 2 | no | types,vm-cli |
| `CRATONVM_DBG_FORCE_MOVING` | 2 | no | types,vm |
| `CRATONVM_DBG_GC_STRESS` | 2 | no | types |
| `CRATONVM_DBG_H2TRACE` | 2 | partial | types,vm |
| `CRATONVM_DBG_HMPUT` | 2 | partial | native-collections |
| `CRATONVM_DBG_JETTY` | 2 | no | types,vm |
| `CRATONVM_DBG_LAMBDA_DISPATCH` | 2 | no | vm |
| `CRATONVM_DBG_LETSGO` | 2 | no | vm |
| `CRATONVM_DBG_LOADER_TRACE` | 2 | no | types,vm |
| `CRATONVM_DBG_NEXTINT` | 2 | no | native-collections,types |
| `CRATONVM_DBG_OSR` | 2 | no | vm |
| `CRATONVM_DBG_SBLOAD` | 2 | partial | native-collections,types |
| `CRATONVM_DBG_SCALAR_NEW` | 2 | no | jit |
| `CRATONVM_DBG_SELECTOR` | 2 | no | types |
| `CRATONVM_DBG_SHADOW` | 2 | no | vm |
| `CRATONVM_DBG_SPID` | 2 | no | jit |
| `CRATONVM_DBG_STALE_OBJREF` | 2 | no | gc,types |
| `CRATONVM_DBG_STW_EXPECTED_IDS` | 2 | no | vm |
| `CRATONVM_DBG_TLS_AUTH` | 2 | no | types |
| `CRATONVM_DBG_TLS_HS` | 2 | no | types |
| `CRATONVM_DBG_VDISP` | 2 | no | types,vm |
| `CRATONVM_GC_VERIFY_STALE` | 2 | partial | vm |
| `CRATONVM_TRACE_UNIMPLEMENTED` | 2 | no | types,vm |
| `CRATONVM_ACTIVE_PROFILES_IDENTITY_TRACE` | 1 | no | vm |
| `CRATONVM_ANN_PROXY_DISPATCH_TRACE` | 1 | no | vm |
| `CRATONVM_ANN_TRACE` | 1 | no | types |
| `CRATONVM_ASSERT_SINGLE_OS_THREAD` | 1 | no | types |
| `CRATONVM_DBG` | 1 | no | types |
| `CRATONVM_DBG_ACCESS` | 1 | no | types |
| `CRATONVM_DBG_AIO` | 1 | no | types |
| `CRATONVM_DBG_AIOOBE2` | 1 | yes | vm |
| `CRATONVM_DBG_AIOOBE3` | 1 | yes | vm |
| `CRATONVM_DBG_ANNPROXY_WRAP` | 1 | no | types |
| `CRATONVM_DBG_ANONALLOC` | 1 | yes | vm |
| `CRATONVM_DBG_AQS_TRACE` | 1 | no | types |
| `CRATONVM_DBG_ARRAYCOPY` | 1 | no | types |
| `CRATONVM_DBG_ARRLEN` | 1 | no | vm |
| `CRATONVM_DBG_ARRSTORE` | 1 | yes | vm |
| `CRATONVM_DBG_ASSERTEQ` | 1 | no | vm |
| `CRATONVM_DBG_ASSERTJ_ARR` | 1 | no | types |
| `CRATONVM_DBG_ATHROW` | 1 | no | vm |
| `CRATONVM_DBG_ATOMIC_UPDATER` | 1 | no | types |
| `CRATONVM_DBG_BADRECV` | 1 | no | vm |
| `CRATONVM_DBG_BADREF` | 1 | no | types |
| `CRATONVM_DBG_BB` | 1 | no | types |
| `CRATONVM_DBG_BBLP` | 1 | no | vm |
| `CRATONVM_DBG_BUFUNDER` | 1 | no | vm |
| `CRATONVM_DBG_BYTECODE_DUMP` | 1 | no | vm |
| `CRATONVM_DBG_CALLER` | 1 | no | types |
| `CRATONVM_DBG_CAPVAL` | 1 | no | types |
| `CRATONVM_DBG_CAUSE` | 1 | no | types |
| `CRATONVM_DBG_CCE` | 1 | no | vm |
| `CRATONVM_DBG_CCECACHE` | 1 | no | types |
| `CRATONVM_DBG_CCSPROBE` | 1 | no | vm |
| `CRATONVM_DBG_CELLCORRUPT` | 1 | no | types |
| `CRATONVM_DBG_CHARSET` | 1 | no | vm |
| `CRATONVM_DBG_CLASSPATH` | 1 | no | types |
| `CRATONVM_DBG_CLONE` | 1 | no | types |
| `CRATONVM_DBG_COERCE` | 1 | no | types |
| `CRATONVM_DBG_COMPACTVALUE` | 1 | no | types |
| `CRATONVM_DBG_COMPACT_LEGACY` | 1 | no | types |
| `CRATONVM_DBG_COMPONENT_TYPE` | 1 | no | types |
| `CRATONVM_DBG_DEFINE` | 1 | no | types |
| `CRATONVM_DBG_DEFLATE` | 1 | no | types |
| `CRATONVM_DBG_DESCTRACE` | 1 | no | types |
| `CRATONVM_DBG_DOPRIV` | 1 | no | types |
| `CRATONVM_DBG_DROPPED_STUBS` | 1 | no | native-api |
| `CRATONVM_DBG_DUMP_JIT` | 1 | no | jit |
| `CRATONVM_DBG_DUPCALL_FILTER` | 1 | no | vm |
| `CRATONVM_DBG_DUPCLASS` | 1 | no | types |
| `CRATONVM_DBG_DUPCLASS_BT` | 1 | no | types |
| `CRATONVM_DBG_DUPX_METHODS` | 1 | no | jit |
| `CRATONVM_DBG_ECWATCH` | 1 | yes | vm |
| `CRATONVM_DBG_ECWATCH_NATIVE` | 1 | yes | vm |
| `CRATONVM_DBG_EQE` | 1 | no | types |
| `CRATONVM_DBG_FBCGLIB` | 1 | no | types |
| `CRATONVM_DBG_FBREF` | 1 | no | types |
| `CRATONVM_DBG_FIELDADDR` | 1 | no | vm |
| `CRATONVM_DBG_FIELD_GET` | 1 | no | types |
| `CRATONVM_DBG_FSP` | 1 | no | types |
| `CRATONVM_DBG_FULLSTACK_SCAN` | 1 | yes | vm |
| `CRATONVM_DBG_FWDGUARD` | 1 | no | types |
| `CRATONVM_DBG_GCPART` | 1 | yes | vm |
| `CRATONVM_DBG_GCPAUSE` | 1 | no | types |
| `CRATONVM_DBG_GCPHASE` | 1 | no | types |
| `CRATONVM_DBG_GCWRITE` | 1 | no | types |
| `CRATONVM_DBG_GC_OVERHEAD` | 1 | no | vm |
| `CRATONVM_DBG_GETRESOURCES` | 1 | no | types |
| `CRATONVM_DBG_GOCBF` | 1 | no | types |
| `CRATONVM_DBG_HANGWALK` | 1 | no | vm |
| `CRATONVM_DBG_HANG_SAMPLE` | 1 | no | vm |
| `CRATONVM_DBG_HEAPCOPY` | 1 | no | vm |
| `CRATONVM_DBG_HEAP_STALE` | 1 | no | vm |
| `CRATONVM_DBG_HEAP_TRACE` | 1 | no | types |
| `CRATONVM_DBG_HEARTBEAT` | 1 | no | vm |
| `CRATONVM_DBG_HOTPATH_COUNTS` | 1 | no | vm |
| `CRATONVM_DBG_HTTPSRV` | 1 | no | types |
| `CRATONVM_DBG_IMSE` | 1 | no | vm |
| `CRATONVM_DBG_INDY_ALL` | 1 | no | vm |
| `CRATONVM_DBG_INDY_GENERIC` | 1 | no | vm |
| `CRATONVM_DBG_INLINE_FR` | 1 | no | jit |
| `CRATONVM_DBG_INVOKESTATS` | 1 | yes | vm |
| `CRATONVM_DBG_INVOKE_COERCE` | 1 | no | types |
| `CRATONVM_DBG_IRSLOT` | 1 | no | jit |
| `CRATONVM_DBG_IR_CALL` | 1 | no | jit |
| `CRATONVM_DBG_IR_LONG` | 1 | no | jit |
| `CRATONVM_DBG_ISINSTANCE` | 1 | no | types |
| `CRATONVM_DBG_JAR` | 1 | no | types |
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
| `CRATONVM_DBG_JLM` | 1 | no | types |
| `CRATONVM_DBG_KCBOOL` | 1 | yes | native-collections |
| `CRATONVM_DBG_LAMBDA` | 1 | no | vm |
| `CRATONVM_DBG_LAMBDA_GENERIC` | 1 | no | types |
| `CRATONVM_DBG_LAYOUT` | 1 | no | types |
| `CRATONVM_DBG_LHM_EVICT` | 1 | no | native-collections |
| `CRATONVM_DBG_LICM` | 1 | no | jit |
| `CRATONVM_DBG_LINKER` | 1 | no | types |
| `CRATONVM_DBG_LOADCLASS` | 1 | no | types |
| `CRATONVM_DBG_LOGPROV` | 1 | no | types |
| `CRATONVM_DBG_LONGROOT` | 1 | yes | vm |
| `CRATONVM_DBG_LOOKUP` | 1 | no | types |
| `CRATONVM_DBG_MCL` | 1 | no | types |
| `CRATONVM_DBG_MEMWATCH` | 1 | yes | vm |
| `CRATONVM_DBG_METHOD_INVOKE_BOX` | 1 | no | types |
| `CRATONVM_DBG_MH_ADAPTER` | 1 | yes | vm |
| `CRATONVM_DBG_MH_DISPATCH` | 1 | no | types |
| `CRATONVM_DBG_MH_STACK` | 1 | yes | vm |
| `CRATONVM_DBG_MIC_PROF` | 1 | yes | vm |
| `CRATONVM_DBG_MINVOKE` | 1 | no | types |
| `CRATONVM_DBG_MODPROV` | 1 | no | types |
| `CRATONVM_DBG_MODSTATIC` | 1 | no | vm |
| `CRATONVM_DBG_MONENTER` | 1 | yes | vm |
| `CRATONVM_DBG_MONEXIT` | 1 | yes | vm |
| `CRATONVM_DBG_MSC` | 1 | no | types |
| `CRATONVM_DBG_MTROOTS` | 1 | yes | vm |
| `CRATONVM_DBG_NCDFE` | 1 | no | vm |
| `CRATONVM_DBG_NET` | 1 | no | types |
| `CRATONVM_DBG_NETTY_QUEUE` | 1 | no | types |
| `CRATONVM_DBG_NIO_BIND` | 1 | no | types |
| `CRATONVM_DBG_NOCODE` | 1 | no | vm |
| `CRATONVM_DBG_NO_CLEANERS` | 1 | yes | vm |
| `CRATONVM_DBG_NO_NONMOVING_RECLAIM` | 1 | no | types |
| `CRATONVM_DBG_NO_PRUNE` | 1 | yes | vm |
| `CRATONVM_DBG_NO_REFPROC` | 1 | yes | vm |
| `CRATONVM_DBG_NPE_INVOKE` | 1 | no | vm |
| `CRATONVM_DBG_NPE_NONE` | 1 | no | vm |
| `CRATONVM_DBG_NPE_STACK` | 1 | no | vm |
| `CRATONVM_DBG_NPE_TRACE` | 1 | no | vm |
| `CRATONVM_DBG_NSME` | 1 | no | vm |
| `CRATONVM_DBG_NULL_NATIVE` | 1 | no | types |
| `CRATONVM_DBG_OBJECTS` | 1 | no | types |
| `CRATONVM_DBG_OBJ_EQUALS` | 1 | no | types |
| `CRATONVM_DBG_OBSREG` | 1 | no | types |
| `CRATONVM_DBG_OSR_META` | 1 | no | jit |
| `CRATONVM_DBG_OVERLAY` | 1 | no | vm |
| `CRATONVM_DBG_OVERLAY_ALL` | 1 | no | vm |
| `CRATONVM_DBG_PARKLAT` | 1 | yes | vm |
| `CRATONVM_DBG_PB` | 1 | no | types |
| `CRATONVM_DBG_PBE` | 1 | no | types |
| `CRATONVM_DBG_PBSTART` | 1 | no | vm |
| `CRATONVM_DBG_PICOCLI_STYLE` | 1 | no | types |
| `CRATONVM_DBG_POPINT` | 1 | no | vm |
| `CRATONVM_DBG_PROXY` | 1 | no | types |
| `CRATONVM_DBG_RAF_GETFD` | 1 | no | types |
| `CRATONVM_DBG_RAF_INIT` | 1 | no | types |
| `CRATONVM_DBG_RE5` | 1 | no | types |
| `CRATONVM_DBG_REFERSTO` | 1 | no | types |
| `CRATONVM_DBG_REFPROC_REMARK` | 1 | no | vm |
| `CRATONVM_DBG_REMAP_TRACE` | 1 | yes | vm |
| `CRATONVM_DBG_REPLOVR` | 1 | no | types |
| `CRATONVM_DBG_RESOLVE_SHIM` | 1 | no | types |
| `CRATONVM_DBG_RESOURCE_TIMING` | 1 | no | types |
| `CRATONVM_DBG_RESUME_PC` | 1 | no | vm |
| `CRATONVM_DBG_ROOTSNAP` | 1 | yes | vm |
| `CRATONVM_DBG_RSET_AUDIT` | 1 | no | types |
| `CRATONVM_DBG_RSET_AUDIT_YOUNG_SCAN` | 1 | no | types |
| `CRATONVM_DBG_RVAS` | 1 | no | libcratonvm |
| `CRATONVM_DBG_SC_CLOSE` | 1 | no | types |
| `CRATONVM_DBG_SC_READ` | 1 | no | types |
| `CRATONVM_DBG_SC_WRITE` | 1 | no | types |
| `CRATONVM_DBG_SEEDHUNT` | 1 | no | types |
| `CRATONVM_DBG_SEED_ALL_OLD` | 1 | no | types |
| `CRATONVM_DBG_SEL` | 1 | no | types |
| `CRATONVM_DBG_SHADOW2` | 1 | yes | jit |
| `CRATONVM_DBG_SHADOW2_FILTER` | 1 | no | jit |
| `CRATONVM_DBG_SHADOW_DEPTH` | 1 | no | vm |
| `CRATONVM_DBG_SHADOW_RELOAD` | 1 | yes | jit |
| `CRATONVM_DBG_SLEEP_TRACE` | 1 | no | types |
| `CRATONVM_DBG_SOCK` | 1 | no | types |
| `CRATONVM_DBG_SOCK_BYTES` | 1 | no | types |
| `CRATONVM_DBG_SOE` | 1 | no | vm |
| `CRATONVM_DBG_STACKLESS` | 1 | yes | vm |
| `CRATONVM_DBG_STALELONG` | 1 | yes | vm |
| `CRATONVM_DBG_STALE_RECV` | 1 | no | vm |
| `CRATONVM_DBG_STREAMSUPP` | 1 | no | types |
| `CRATONVM_DBG_STW_NATIVE_RING` | 1 | no | vm |
| `CRATONVM_DBG_SWEEP_CENSUS` | 1 | no | types |
| `CRATONVM_DBG_SWEEP_EDGES` | 1 | no | types |
| `CRATONVM_DBG_SWEEP_ZERO` | 1 | no | types |
| `CRATONVM_DBG_THREADREG_PERF` | 1 | yes | vm |
| `CRATONVM_DBG_THREADSTART` | 1 | no | vm |
| `CRATONVM_DBG_TIER_ENQUEUE` | 1 | no | jit |
| `CRATONVM_DBG_TLS_PLS` | 1 | no | types |
| `CRATONVM_DBG_TLS_SOCK` | 1 | no | types |
| `CRATONVM_DBG_TLS_SRV` | 1 | no | types |
| `CRATONVM_DBG_TOHEX` | 1 | no | types |
| `CRATONVM_DBG_UCLREG` | 1 | no | types |
| `CRATONVM_DBG_UCLRES` | 1 | no | types |
| `CRATONVM_DBG_UNCAUGHT` | 1 | no | vm |
| `CRATONVM_DBG_UNDERFLOW` | 1 | no | vm |
| `CRATONVM_DBG_UNPARK_MISS` | 1 | no | vm |
| `CRATONVM_DBG_UNPIN_RING` | 1 | yes | vm |
| `CRATONVM_DBG_URLCL` | 1 | no | types |
| `CRATONVM_DBG_UTE` | 1 | no | types |
| `CRATONVM_DBG_VALIDATE_NEW` | 1 | no | vm |
| `CRATONVM_DBG_VERIFY_ERROR` | 1 | no | vm |
| `CRATONVM_DBG_VERIFY_INLINE_FRAME_RECORD` | 1 | yes | jit |
| `CRATONVM_DBG_VERIFY_OOP_MAPS` | 1 | yes | vm |
| `CRATONVM_DBG_VISITFILE` | 1 | no | types |
| `CRATONVM_DBG_VM_STATE` | 1 | yes | vm |
| `CRATONVM_DBG_WATCHADDR` | 1 | yes | vm |
| `CRATONVM_DBG_WATCH_CAUSE_SELF` | 1 | no | types |
| `CRATONVM_DBG_WEAKREF` | 1 | yes | vm |
| `CRATONVM_DBG_WF` | 1 | no | types |
| `CRATONVM_DBG_WF_NPE` | 1 | no | vm |
| `CRATONVM_DBG_XNIO_TCP` | 1 | no | types |
| `CRATONVM_DBG_YOUNGSCAN` | 1 | yes | vm |
| `CRATONVM_DBG_YOUNGSTATE` | 1 | no | types |
| `CRATONVM_DBG_ZERO_RANGES` | 1 | no | types |
| `CRATONVM_DEBUG_SFI` | 1 | no | types |
| `CRATONVM_DEBUG_STACKWALK` | 1 | no | types |
| `CRATONVM_DEBUG_STACK_TAG` | 1 | no | vm |
| `CRATONVM_DEOPT_VERIFY` | 1 | yes | jit |
| `CRATONVM_DIAG_HIB32` | 1 | no | types |
| `CRATONVM_DIAG_JAR_LIST` | 1 | no | types |
| `CRATONVM_DIAG_JBOSS_SERVICES` | 1 | no | types |
| `CRATONVM_DIAG_JCA` | 1 | no | types |
| `CRATONVM_DIAG_METHOD_INVOKE_NULL` | 1 | no | types |
| `CRATONVM_DIAG_PROPERTIES` | 1 | no | types |
| `CRATONVM_DIAG_SERVICELOADER` | 1 | no | types |
| `CRATONVM_ENABLE_ASSERTIONS` | 1 | no | types |
| `CRATONVM_EXEC_FRAME_TRACE` | 1 | no | vm |
| `CRATONVM_FORNAME_TRACE` | 1 | no | types |
| `CRATONVM_FRAME_TRACE` | 1 | no | vm |
| `CRATONVM_FWD_RESOLVE_STRICT` | 1 | no | types |
| `CRATONVM_G1_DBG_HEADERS` | 1 | no | types |
| `CRATONVM_G1_DBG_PINS` | 1 | no | types |
| `CRATONVM_G1_DBG_REACH` | 1 | no | types |
| `CRATONVM_G1_DBG_ROOTCENSUS` | 1 | no | types |
| `CRATONVM_G1_DBG_ZERO` | 1 | no | types |
| `CRATONVM_GC_ARRAY_GUARD_BT` | 1 | no | types |
| `CRATONVM_GC_STATS` | 1 | no | vm-cli |
| `CRATONVM_GPU_TRACE_BYTES` | 1 | yes | vm |
| `CRATONVM_HM_TRACE` | 1 | yes | native-collections |
| `CRATONVM_HS_ITR_DBG` | 1 | yes | native-collections |
| `CRATONVM_IAE_TRACE2` | 1 | no | types |
| `CRATONVM_INTRINSIC_STATS` | 1 | no | vm-cli |
| `CRATONVM_INVOKESTATIC_LOADER_TRACE` | 1 | yes | vm |
| `CRATONVM_INVOKE_VIRTUAL_ENTRY_TRACE` | 1 | yes | vm |
| `CRATONVM_JBOSS_BOOT_LOG_FILE` | 1 | no | types |
| `CRATONVM_JBOSS_LOGGER_BASE_EMIT` | 1 | no | types |
| `CRATONVM_JIT_BISECT_ONLY` | 1 | yes | vm |
| `CRATONVM_JIT_BISECT_SKIP` | 1 | yes | vm |
| `CRATONVM_LDC_CLASSREF_TRACE` | 1 | no | vm |
| `CRATONVM_LOCK_ORDER_CHECK` | 1 | no | types |
| `CRATONVM_LONGROOT_STRICT` | 1 | yes | vm |
| `CRATONVM_MOVING_YOUNG_COVERAGE_DBG` | 1 | no | vm |
| `CRATONVM_MOVING_YOUNG_VERIFY` | 1 | no | types |
| `CRATONVM_NEEDS_EXACT_TRACE` | 1 | no | vm |
| `CRATONVM_NO_SELECTOR_CONNECT_PROBE` | 1 | no | types |
| `CRATONVM_NSEE_TRACE` | 1 | no | vm |
| `CRATONVM_OOP_SPAN_PROBE` | 1 | no | types |
| `CRATONVM_QUICKEN_STATS` | 1 | yes | reader |
| `CRATONVM_S111_DBG` | 1 | no | types |
| `CRATONVM_SFI_NULL_TRACE` | 1 | no | types |
| `CRATONVM_SHADOW_SENTINEL` | 1 | yes | jit |
| `CRATONVM_SHADOW_WATCH` | 1 | yes | jit |
| `CRATONVM_SPRING_DBG` | 1 | no | types |
| `CRATONVM_SP_STATS` | 1 | no | types |
| `CRATONVM_SP_TRACE` | 1 | no | types |
| `CRATONVM_SP_VERIFY` | 1 | no | types |
| `CRATONVM_STRICT_SWALLOWS` | 1 | yes | vm |
| `CRATONVM_SUREFIRE_IPC_DBG` | 1 | no | types |
| `CRATONVM_SYMBOLIZE` | 1 | no | vm-cli |
| `CRATONVM_SYMBOLIZE_DBG` | 1 | no | vm |
| `CRATONVM_TRACE_ARRAYS_HASHCODE` | 1 | no | types |
| `CRATONVM_TRACE_PTI_ARGS` | 1 | no | types |
| `CRATONVM_TRACE_SB_FILTER` | 1 | no | vm |
| `CRATONVM_TRACK_NATIVE` | 1 | yes | native-api |
| `CRATONVM_UEH_DEBUG` | 1 | no | types |

## 6. Class (c) — test-only flags

| Flag | Reads | Crates | Note |
| --- | ---: | --- | --- |
| `CRATONVM_BIN` | 86 | difftest,vm | vm/tests/sb_count_slot_guard_regression.rs:43 |
| `CRATONVM_DIFF_HOTSPOT` | 1 | vm | vm/tests/jit_interp_differential.rs:487 |
| `CRATONVM_JAVA_HOME` | 30 | native-builtins,vm | native-builtins/src/phases_late/nio_file.rs:7975 |
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
| `CRATONVM_TEST_JDK` | 13 | native-builtins,vm | native-builtins/src/phases_late/nio_file.rs:7975 |
| `CRATONVM_TEST_SEGV` | 1 | vm-cli | vm-cli/src/main.rs:3559 |
| `CRATONVM_TEST_VAR` | 1 | vm | vm/src/vm.rs:11560 |

## 7. `native-builtins/` — migrated (T3.5b)

Before T3.5b this crate held **359** direct read sites across **156** flag names,
**127** of which were read only there. It was deliberately deferred while it was
being split from a single 86 000-line `lib.rs` into per-domain modules; T3.5b
migrated it once the split landed. It now names a flag on
**15** lines in total.

**11** direct `std::env::var` / `var_os` sites remain, down from 359. Every
one of them is a flag whose value is mutated **in-process** with `set_var` /
`remove_var` and then re-read — a latched typed config cannot reproduce that, so
migrating them would be a silent behaviour change rather than plumbing:

| Flag | Direct sites left | Why it stays on `std::env` |
| --- | ---: | --- |
| `CRATONVM_JBOSS_MP_ROOT` | 8 | `find_mp_argument()` is exercised by unit tests that `set_var` / `remove_var` it in-process and assert the read changes (`jboss_module_loader.rs:3880`, `:3897`, `lib.rs:2383`..`:2478`) |
| `CRATONVM_MAVEN_REPO_LOCAL` | 1 | `jboss_module_loader.rs:4034` sets it in-process, then calls `resolve_module()` and asserts the new repo is used |
| `CRATONVM_SYNTHETIC_QUARKUS_ARC` | 2 | `quarkus_arc.rs:1330`/`:1345` flips it in-process and asserts `synthetic_arc_opted_in()` follows |

The other 348 sites now read `cratonvm_types::flags()`: the flags read only by
this crate live on `VmFlags::natives` (`NativeFlags`), and the eight it shares
with `gc` / `classloading` / `native-io` reuse those crates' existing fields
rather than being duplicated. Four parsers had to be added for truth tables no
existing parser matched — see §10, where the count went from seven to eleven.

The migration also absorbed the three local `OnceLock<bool>` helpers added by
`c258662e4` (`h2trace_enabled`, and two byte-identical copies of
`loader_trace_enabled`). They existed because `native-builtins` cannot reach
`vm`, where `env_cache` lives — but it *can* reach `types`, so the typed config
subsumes them. The call sites keep the flag read as the LEFT operand of their
`&&`, which is what that fix required.

Once a crate is migrated the scan can no longer tell which flags *belong* to
it — the call sites no longer name them. So the catalogue below is read back
out of `NativeFlags::from_source`, which is the new home of record. It is the
same evidence the pre-migration §7 table carried, with the parser each field
was built from in place of the read count.

| Flag | `NativeFlags` field | Parser |
| --- | --- | --- |
| `CRATONVM_ANN_TRACE` | `ann_trace` | `present_utf8` |
| `CRATONVM_ASYNC_HANDOFF_SLEEP_FLOOR_MS` | `async_handoff_sleep_floor_ms` | `i64_opt_untrimmed` |
| `CRATONVM_ASYNC_SUBMIT_GRACE_MS` | `async_submit_grace_ms` | `u64_opt_untrimmed` |
| `CRATONVM_ASYNC_WORKER_SLEEP_FLOOR_MS` | `async_worker_sleep_floor_ms` | `i64_opt_untrimmed` |
| `CRATONVM_AWAIT_NO_SHORTCIRCUIT` | `await_no_shortcircuit` | `present` |
| `CRATONVM_BD_DEBUG` | `bd_debug` | `present` |
| `CRATONVM_CANON_OPENFILE` | `canon_openfile` | `present` |
| `CRATONVM_CL_BOOTSTRAP_SCOPED` | `cl_bootstrap_scoped` | `on_unless_zero` |
| `CRATONVM_DBG` | `dbg` | `present` |
| `CRATONVM_DBG_ANNPROXY_WRAP` | `dbg_annproxy_wrap` | `present` |
| `CRATONVM_DBG_AQS_TRACE` | `dbg_aqs_trace` | `present` |
| `CRATONVM_DBG_ARRAYCOPY` | `dbg_arraycopy` | `exactly_one` |
| `CRATONVM_DBG_ASSERTJ_ARR` | `dbg_assertj_arr` | `present` |
| `CRATONVM_DBG_ATOMIC_UPDATER` | `dbg_atomic_updater` | `present` |
| `CRATONVM_DBG_BB` | `dbg_bb` | `present_utf8` |
| `CRATONVM_DBG_CALLER` | `dbg_caller` | `present_utf8` |
| `CRATONVM_DBG_CAPVAL` | `dbg_capval` | `present` |
| `CRATONVM_DBG_CATALINA` | `dbg_catalina` | `present_utf8` |
| `CRATONVM_DBG_CAUSE` | `dbg_cause` | `present` |
| `CRATONVM_DBG_CCECACHE` | `dbg_ccecache` | `present` |
| `CRATONVM_DBG_CCE_BT` | `dbg_cce_bt` | `present` |
| `CRATONVM_DBG_CLONE` | `dbg_clone` | `present` |
| `CRATONVM_DBG_COERCE` | `dbg_coerce` | `present` |
| `CRATONVM_DBG_COMPONENT_TYPE` | `dbg_component_type` | `present_utf8` |
| `CRATONVM_DBG_DEFLATE` | `dbg_deflate` | `present` |
| `CRATONVM_DBG_DOPRIV` | `dbg_dopriv` | `present` |
| `CRATONVM_DBG_EQE` | `dbg_eqe` | `present` |
| `CRATONVM_DBG_EXEC` | `dbg_exec` | `present` |
| `CRATONVM_DBG_EXIT` | `dbg_exit` | `exactly_one` |
| `CRATONVM_DBG_FBREF` | `dbg_fbref` | `present` |
| `CRATONVM_DBG_FIELD_GET` | `dbg_field_get` | `present_utf8` |
| `CRATONVM_DBG_FSP` | `dbg_fsp` | `present_utf8` |
| `CRATONVM_DBG_GOCBF` | `dbg_gocbf` | `present` |
| `CRATONVM_DBG_H2TRACE` | `dbg_h2trace` | `present` |
| `CRATONVM_DBG_HTTPSRV` | `dbg_httpsrv` | `present` |
| `CRATONVM_DBG_INVOKE_COERCE` | `dbg_invoke_coerce` | `exactly_one` |
| `CRATONVM_DBG_ISINSTANCE` | `dbg_isinstance` | `present` |
| `CRATONVM_DBG_JLM` | `dbg_jlm` | `present_utf8` |
| `CRATONVM_DBG_LAMBDA_GENERIC` | `dbg_lambda_generic` | `present_utf8` |
| `CRATONVM_DBG_LINKER` | `dbg_linker` | `present` |
| `CRATONVM_DBG_LOADER_TRACE` | `dbg_loader_trace` | `present` |
| `CRATONVM_DBG_LOGPROV` | `dbg_logprov` | `present` |
| `CRATONVM_DBG_LOOKUP` | `dbg_lookup` | `present_utf8` |
| `CRATONVM_DBG_MCL` | `dbg_mcl` | `present` |
| `CRATONVM_DBG_METHOD_INVOKE_BOX` | `dbg_method_invoke_box` | `present` |
| `CRATONVM_DBG_MH_DISPATCH` | `dbg_mh_dispatch` | `present` |
| `CRATONVM_DBG_MINVOKE` | `dbg_minvoke` | `present` |
| `CRATONVM_DBG_MSC` | `dbg_msc` | `present` |
| `CRATONVM_DBG_NETTY_QUEUE` | `dbg_netty_queue` | `present` |
| `CRATONVM_DBG_NEXTINT` | `dbg_nextint` | `present_utf8` |
| `CRATONVM_DBG_NIO_BIND` | `dbg_nio_bind` | `present` |
| `CRATONVM_DBG_NULL_NATIVE` | `dbg_null_native` | `present` |
| `CRATONVM_DBG_OBJECTS` | `dbg_objects` | `present` |
| `CRATONVM_DBG_OBJ_EQUALS` | `dbg_obj_equals` | `present` |
| `CRATONVM_DBG_PBE` | `dbg_pbe` | `present` |
| `CRATONVM_DBG_PICOCLI_STYLE` | `dbg_picocli_style` | `present_utf8` |
| `CRATONVM_DBG_PROXY` | `dbg_proxy` | `present_utf8` |
| `CRATONVM_DBG_RAF_GETFD` | `dbg_raf_getfd` | `present` |
| `CRATONVM_DBG_RAF_INIT` | `dbg_raf_init` | `present` |
| `CRATONVM_DBG_RE5` | `dbg_re5` | `present` |
| `CRATONVM_DBG_REFERSTO` | `dbg_refersto` | `present` |
| `CRATONVM_DBG_REFLECTION_FACTORY` | `dbg_reflection_factory` | `present` |
| `CRATONVM_DBG_REPLOVR` | `dbg_replovr` | `present` |
| `CRATONVM_DBG_RESOLVE_SHIM` | `dbg_resolve_shim` | `present` |
| `CRATONVM_DBG_SBLOAD` | `dbg_sbload` | `present` |
| `CRATONVM_DBG_SEL` | `dbg_sel` | `present` |
| `CRATONVM_DBG_SLEEP_TRACE` | `dbg_sleep_trace` | `present` |
| `CRATONVM_DBG_SOCK` | `dbg_sock` | `present` |
| `CRATONVM_DBG_SOCK_BYTES` | `dbg_sock_bytes` | `present` |
| `CRATONVM_DBG_STREAMSUPP` | `dbg_streamsupp` | `present_utf8` |
| `CRATONVM_DBG_STTRACE` | `dbg_sttrace` | `present` |
| `CRATONVM_DBG_TLS_AUTH` | `dbg_tls_auth` | `present` |
| `CRATONVM_DBG_TLS_AUTH` | `dbg_tls_auth_ok` | `present_utf8` |
| `CRATONVM_DBG_TLS_HS` | `dbg_tls_hs` | `present` |
| `CRATONVM_DBG_TLS_HS` | `dbg_tls_hs_ok` | `present_utf8` |
| `CRATONVM_DBG_TLS_PLS` | `dbg_tls_pls` | `present` |
| `CRATONVM_DBG_TLS_SOCK` | `dbg_tls_sock` | `present` |
| `CRATONVM_DBG_TLS_SRV` | `dbg_tls_srv` | `present` |
| `CRATONVM_DBG_TOARRAY` | `dbg_toarray` | `present` |
| `CRATONVM_DBG_TOARRAY` | `dbg_toarray_ok` | `present_utf8` |
| `CRATONVM_DBG_TOHEX` | `dbg_tohex` | `present` |
| `CRATONVM_DBG_UCLREG` | `dbg_uclreg` | `present` |
| `CRATONVM_DBG_UCLRES` | `dbg_uclres` | `present` |
| `CRATONVM_DBG_URLCL` | `dbg_urlcl` | `present` |
| `CRATONVM_DBG_UTE` | `dbg_ute` | `present` |
| `CRATONVM_DBG_VDISP` | `dbg_vdisp` | `present` |
| `CRATONVM_DBG_VISITFILE` | `dbg_visitfile` | `present` |
| `CRATONVM_DBG_WATCH_CAUSE_SELF` | `dbg_watch_cause_self` | `utf8` |
| `CRATONVM_DBG_WF` | `dbg_wf` | `present` |
| `CRATONVM_DBG_XNIO_TCP` | `dbg_xnio_tcp` | `present` |
| `CRATONVM_DEBUG_SFI` | `debug_sfi` | `present` |
| `CRATONVM_DEBUG_STACKWALK` | `debug_stackwalk` | `present` |
| `CRATONVM_DIAG_JBOSS_SERVICES` | `diag_jboss_services` | `utf8` |
| `CRATONVM_DIAG_JCA` | `diag_jca` | `present` |
| `CRATONVM_DIAG_METHOD_INVOKE_NULL` | `diag_method_invoke_null` | `present` |
| `CRATONVM_DIAG_PROPERTIES` | `diag_properties` | `one_true_yes_exact` |
| `CRATONVM_DIAG_SERVICELOADER` | `diag_serviceloader` | `one_true_yes_exact` |
| `CRATONVM_ENABLE_ASSERTIONS` | `enable_assertions` | `present` |
| `CRATONVM_EQE_SYNC_EXECUTE` | `eqe_sync_execute` | `present` |
| `CRATONVM_FORNAME_TRACE` | `forname_trace` | `present` |
| `CRATONVM_HTTP_MAX_BODY` | `http_max_body` | `usize_positive` |
| `CRATONVM_IAE_TRACE` | `iae_trace` | `present` |
| `CRATONVM_IAE_TRACE` | `iae_trace_ok` | `present_utf8` |
| `CRATONVM_IAE_TRACE2` | `iae_trace2` | `present_utf8` |
| `CRATONVM_INHERIT_THREAD_CCL` | `inherit_thread_ccl` | `on_unless_zero_or_false` |
| `CRATONVM_INHERIT_TL_WORKAROUND` | `inherit_tl_workaround` | `on_unless_zero_or_false` |
| `CRATONVM_JBOSS_BOOT_LOG_FILE` | `jboss_boot_log_file` | `utf8` |
| `CRATONVM_JBOSS_BRUTE_FORCE_JARS` | `jboss_brute_force_jars` | `exactly_one` |
| `CRATONVM_JBOSS_LOGGER_BASE_EMIT` | `jboss_logger_base_emit` | `exactly_one` |
| `CRATONVM_LOADER_UNLOAD` | `loader_unload` | `on_unless_zero` |
| `CRATONVM_MAX_INFLATED_BYTES` | `max_inflated_bytes` | `utf8` |
| `CRATONVM_MSC_REAL_START` | `msc_real_start` | `on_unless_off_word_cased` |
| `CRATONVM_NATIVE_EC_MULTIPLY` | `native_ec_multiply` | `present` |
| `CRATONVM_NATIVE_MATCHER_FIND` | `native_matcher_find` | `on_unless_zero_or_false` |
| `CRATONVM_NATIVE_PBE_KEYFACTORY` | `native_pbe_keyfactory` | `exactly_one` |
| `CRATONVM_NATIVE_STRING_REGEX` | `native_string_regex` | `on_unless_zero_or_false` |
| `CRATONVM_NETTY_QUEUE_BRIDGE` | `netty_queue_bridge` | `on_unless_zero_or_false` |
| `CRATONVM_REAL_AGROAL` | `real_agroal` | `present` |
| `CRATONVM_REAL_ANNOTATIONS` | `real_annotations` | `on_unless_off_word_cased` |
| `CRATONVM_REAL_AQS` | `real_aqs` | `present` |
| `CRATONVM_REAL_JCA` | `real_jca` | `present` |
| `CRATONVM_REAL_PROXY` | `real_proxy` | `truthy_word_default_true` |
| `CRATONVM_REAL_PROXY_STRICT` | `real_proxy_strict` | `affirmative_word` |
| `CRATONVM_REAL_PROXY_SUPER` | `real_proxy_super` | `truthy_word_default_true` |
| `CRATONVM_REAL_PROXY_SUPER` | `real_proxy_super_set` | `present` |
| `CRATONVM_REAL_QUARKUS_START` | `real_quarkus_start` | `present` |
| `CRATONVM_REAL_STAX_FACTORY` | `real_stax_factory` | `on_unless_zero` |
| `CRATONVM_REAL_VERTX` | `real_vertx` | `present` |
| `CRATONVM_REQUIRE_POLICY` | `require_policy` | `present` |
| `CRATONVM_S111_DBG` | `s111_dbg` | `present_utf8` |
| `CRATONVM_SFI_NULL_TRACE` | `sfi_null_trace` | `present` |
| `CRATONVM_SOFT_EXIT` | `soft_exit` | `exactly_one` |
| `CRATONVM_SPRING_DBG` | `spring_dbg` | `present` |
| `CRATONVM_SYNTHETIC_AGROAL` | `synthetic_agroal` | `present` |
| `CRATONVM_SYNTHETIC_ANNOTATIONS` | `synthetic_annotations` | `present` |
| `CRATONVM_SYNTHETIC_AQS` | `synthetic_aqs` | `present` |
| `CRATONVM_SYNTHETIC_BUFFERED_WRITER` | `synthetic_buffered_writer` | `exactly_one` |
| `CRATONVM_SYNTHETIC_DSA` | `synthetic_dsa` | `present` |
| `CRATONVM_SYNTHETIC_EC` | `synthetic_ec` | `present` |
| `CRATONVM_SYNTHETIC_EQE` | `synthetic_eqe` | `present_utf8` |
| `CRATONVM_SYNTHETIC_PQC` | `synthetic_pqc` | `present` |
| `CRATONVM_SYNTHETIC_RSA` | `synthetic_rsa` | `present` |
| `CRATONVM_SYNTHETIC_VERTX` | `synthetic_vertx` | `present` |
| `CRATONVM_TRACE_ARRAYS_HASHCODE` | `trace_arrays_hashcode` | `present` |
| `CRATONVM_TRACE_CLASSVALUE` | `trace_classvalue` | `present` |
| `CRATONVM_TRACE_PTI_ARGS` | `trace_pti_args` | `present` |
| `CRATONVM_UEH_DEBUG` | `ueh_debug` | `present` |
| `CRATONVM_URI_STRICT_CHARS` | `uri_strict_chars` | `on_unless_zero` |
| `CRATONVM_USE_WILDFLY_REFLECT_SHIM` | `use_wildfly_reflect_shim` | `exactly_one` |
| `CRATONVM_USE_WILDFLY_SYNTH_BYTECODE` | `use_wildfly_synth_bytecode` | `exactly_one` |

## 8. Caching status and the per-call readers

`std::env::var` takes a process-global lock in libc `getenv` and allocates. Of the
928 read sites, **464** are not behind a `OnceLock`. Most of those are cold
(startup, class load, JIT compile), but three are genuinely hot and are the reason
the typed config is worth doing on performance grounds alone:

| Site | Frequency | Note |
| --- | --- | --- |
| `native-builtins/src/lang_system.rs:752` `CRATONVM_INHERIT_THREAD_CCL` | once per `Thread.start0` | **migrated** in T3.5b — now `flags().natives.inherit_thread_ccl` |
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

## 10. Finding: eleven disagreeing boolean truth tables

There is no single answer to "what does `CRATONVM_FOO=false` mean". The tree
contains at least these eleven, all of them live. The first four were found by
the census scan; three more surfaced while migrating `classloading` and
`native-io`; the last four surfaced while migrating `native-builtins`. The
count has gone up at every single migration, which is the finding: it is a
lower bound, not a total.

| Parser | `unset` | `""` | `"0"` | `"false"` | `"off"` | `"no"` | `"NO"` | else |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `var_os(..).is_some()` — the ~600-site majority | false | **true** | **true** | true | true | true | true | true |
| `env_cache::disable_jit` (`env_cache.rs:70`) | false | **false** | **false** | true | true | true | true | true |
| `native_io::env_flag_enabled` (was `lib.rs:168`) | false | false | false | **false** | **false** | **false** | **false** | true |
| `TieredParams::tiered_enabled` (`tiered.rs:223`) | **true** | **true** | false | false | **true** | **true** | **true** | true |
| `lock_order::compute_enforced` (`lock_order.rs:242`) | false | false | false | false | false | false | false | **only `1`/`true`/`yes`/`on`** |
| `class_manager::loader_aware_resolution` (`:141`) | **true** | false | false | true | true | true | true | true |
| `class_path::dbg_getresources` + `nio_selector::sel_dbg_enabled` | false | false | false | *differs*: `dbg_getresources` true, `sel_dbg_enabled` **false** | true | true | true | true |
| `service_loader::diag_serviceloader` (`one_true_yes_exact`) | false | false | false | false | **false** | false | false | **only exact `1`/`true`/`yes`** |
| `lang_system::inherit_thread_ccl` (`on_unless_zero_or_false`) | **true** | **true** | false | false | **true** | **true** | **true** | true |
| `jboss_msc::msc_real_start` (`on_unless_off_word_cased`) | **true** | **true** | false | false | false | **true** | **true** | true |
| `reflect_annotations::real_proxy_enabled` (`truthy_word_default_true`) | **true** | **true** | false | false | false | false | false | true |

The four `native-builtins` additions are not near-duplicates of the earlier
seven. `on_unless_zero_or_false` and `truthy_word_default_true` disagree with
each other on `off` and `no`; `on_unless_off_word_cased` accepts `off`/`OFF`
but not `Off`, while `on_unless_off_word` accepts `off` but not `OFF`; and
`one_true_yes_exact` rejects `on`, which `affirmative_word` accepts, and
rejects `" 1 "`, which `affirmative_word` trims and accepts.

So `CRATONVM_X=0` *enables* the feature at roughly 600 sites and *disables* it
at six others, and `CRATONVM_X=false` splits two flags that look like siblings.
This is a genuine footgun and the single strongest argument for one typed
config: the parse happens once, in one place, and each field records which
table it uses.

**No branch has unified the truth tables.** Each migrated flag keeps its own
parse function byte-for-byte, because changing what `X=0` means for 600 flags
is a behaviour change, not a plumbing change. All eleven now live side by side
in `cratonvm_types::flags::parse`, each documented with the call site it was
lifted from, and two unit tests
(`boolean_parsers_disagree_exactly_as_documented`,
`native_builtins_parsers_add_four_more_truth_tables`) assert that they still
disagree — so the divergence cannot be tidied away by accident and can instead
be retired deliberately, flag by flag, with benchmarks.

## 11. Migration status

Read sites still calling `std::env::var` / `var_os` directly, by crate.
A crate at zero reads every flag from `cratonvm_types::flags()`.

Counted here are only sites with a literal `std::env::var` / `var_os` call —
the thing the migration removes. A flag name that merely appears in an
assertion message or as a label argument is not one, and `types/src/flags.rs`,
where the parse now lives, is excluded by construction.

| Crate | Read sites remaining | Status |
| --- | ---: | --- |
| `classloading` | 0 | **migrated** |
| `difftest` | 1 | not started |
| `fuzz` | 1 | not started |
| `gc` | 3 | **INCOMPLETE** |
| `jit` | 112 | not started |
| `libcratonvm` | 5 | not started |
| `native-api` | 5 | not started |
| `native-builtins` | 11 | **migrated** — 11 holdouts, all in-process `set_var` targets; see §7 |
| `native-collections` | 16 | not started |
| `native-io` | 0 | **migrated** |
| `reader` | 1 | not started |
| `types` | 7 | not started |
| `vm` | 342 | not started |
| `vm-cli` | 18 | not started |

`native-builtins` is listed as migrated with a non-zero count on purpose. The
eleven remaining sites read a variable that something in the same process
rewrites with `set_var` before re-reading it; a config latched once at startup
cannot express that. Retiring them means retiring the in-process mutation, not
plumbing the read differently.

## 12. Reproducing this census

```sh
python3 tools/flag-census/census.py            # totals, from the repo root
python3 tools/flag-census/census.py /path/to/repo
```

The scan is deliberately literal-only: there is **no** dynamic env-var name
construction anywhere in the workspace (verified: no `format!("CRATONVM_{}", ..)`
and no `env::var(<non-literal>)` outside test JDK-discovery helpers and the three
injectable helpers named in §9), so a literal scan is exhaustive. `census.py`
re-checks that invariant on every run and aborts if it is ever violated.

