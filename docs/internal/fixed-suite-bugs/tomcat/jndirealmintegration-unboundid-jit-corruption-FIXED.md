# TestJNDIRealmIntegration — UnboundID cross-package JIT corruption (FIXED)

**Status:** FIXED on `fix/tomcat-jndirealm-unboundid-jit-20260726`. Both JIT
guards removed: **TOMCAT-JNDIREALM-RDN.1** (2026-07-15,
`com/unboundid/ldap/sdk/RDN.getNameValuePairs`) and **TOMCAT-JNDIREALM-JIT.2**
(2026-07-23, all of `com/unboundid/`).

Supersedes `docs/known-issues/springboot/tomcat-jndirealm-unboundid-jit-corruption.md`
and closes the "Diagnostic follow-up" thread in
[jndirealmintegration-specialchar-credential-residual-FIXED.md](jndirealmintegration-specialchar-credential-residual-FIXED.md).

**Affected test:** `org.apache.catalina.realm.TestJNDIRealmIntegration`
(76-case parameterized matrix, real UnboundID in-memory LDAP server, real
sockets, real JDK 25).

## Symptom

With the guards lifted (`CRATONVM_JIT_ALLOW_PACKAGES=com/unboundid/`), the
default JIT intermittently lost a live `String` on the embedded LDAP server's
DN/RDN matching path:

```
WARN Stale pointer detected in invokevirtual receiver
     (ptr=0x…, all-zero header) — falling back to CP class java/lang/String
LDAP: error code 80 … ClassCastException(java.lang.Object cannot be cast to
     java.lang.String)
java.lang.AssertionError: expected:<1> but was:<0>   (authentication group)
```

`--nojit` and HotSpot were clean, so this was never an LDAP or native-JNDI
contract issue.

## Root cause — TOMCAT-JNDIREALM-JIT.3

`JvmThread::string_case_cache` (the ASCII case-conversion cache: three raw
`ObjectRef`s per entry — `source`, `first`, `second`, alternating so
consecutive conversions stay observably distinct, up to 32 entries) was wired
into only **two** of the VM's root/remap paths:

| # | Path | `jit_hashmap_string_node_cache` | `string_case_cache` (before) |
|---|------|---|---|
| 1 | `memory/roots.rs` `collect_roots` — scan | yes | yes |
| 2 | `memory/gc.rs` `update_all_roots` — remap | yes | yes |
| 3 | `runtime/interpreter.rs` `update_root_snapshot` — safepoint publish | yes | **no** |
| 4 | `runtime/interpreter.rs` `apply_pointer_map_to_thread` — safepoint remap | yes | **no** |
| 5 | `vm/vm_exec.rs` `deposit_root_snapshot_inner` — blocked publish | yes | **no** |
| 6 | `vm/vm_exec.rs` `check_post_block_gc_refs` — blocked-wake remap | yes | **no** |
| 7 | `runtime/interpreter.rs` frozen-peer walk (xt takeover) | **no** | **no** |

`collect_roots` runs **only for the GC initiator**. Every other thread is
marked exclusively from its *deposited snapshot*, which scans frames — and
these cache entries live in no frame. So a collection initiated by any other
thread could not see them: the non-moving young sweep reclaimed the cached
`first`/`second` Strings, and the owning thread's next
`get_ascii_case_string_cached` handed the freed address straight back to
bytecode as an all-zero-header `java/lang/String` receiver.

The in-memory LDAP server is a thread-per-connection design, so a GC is almost
always initiated by a *different* thread than the one holding the cache — which
is why this reproduced so readily here and why it is JIT-dependent: the cache is
consulted from the compiled path (`vm/src/jit/helpers.rs`, and
`native-builtins/src/lang_string.rs` for the native path).

## Narrowing evidence

- **One trigger method.** `CRATONVM_JIT_BISECT_SKIP=com/unboundid/util/StaticUtils.toLowerCase`
  alone took the reclaimed-live-String count from 3–4 per run to **0**, with
  every other UnboundID class still compiled. That method's entire body is
  `aload_0; ifnonnull; getstatic Locale.ROOT; invokevirtual String.toLowerCase(Locale); areturn`
  — i.e. its only work is the case conversion whose per-thread result cache was
  the unrooted holder.
- **Both victims were that call's result.** `CRATONVM_DBG_STALE_RECV` put the
  stale slot at `SearchEntryParer.getNameWithOptions` `local[1]` (pc 11, the
  `astore_1` of `StaticUtils.toLowerCase`) and
  `MatchingRule.selectEqualityMatchingRule` `local[0]`/`local[1]` (pc 27, same
  call at pc 16).
- **Reclaimed, not relocated.** `CRATONVM_DBG_A2` showed `ALLOC` then `FREE` for
  the exact address; `CRATONVM_DBG_GCPART` showed `moved_to=None` in every
  epoch; `CRATONVM_DBG_SWEEP_ZERO` named it
  `RECLAIMED-LIVE receiver … original class=java/lang/String … zeroed by
  non-moving sweep cycle N`, with `initiator_tid` ≠ `holder tid` every time.
  A missed **mark**, on a non-initiator thread.
- **Not a JIT-frame/oop-map gap.** None of these changed the event count, which
  is itself the tell that the reference was never in a frame or register:
  `CRATONVM_NO_PRECISE_JIT_MAPS`, `CRATONVM_DBG_FULLSTACK_SCAN`,
  `CRATONVM_XT_JIT_ROOT_SCAN=0`, `CRATONVM_XT_HELPER_WINDOW_SCAN=0`,
  `CRATONVM_SHADOW_PIN`, `CRATONVM_REAL_FORKJOINPOOL` (conservative locals),
  `CRATONVM_ROOTSNAP_CACHE`, `CRATONVM_REAL_AQS`.

## Fix

Publish and remap `string_case_cache` in the four missing paths, mirroring
`jit_hashmap_string_node_cache` exactly, and add both caches to the frozen-peer
walk (path 7), which previously contributed only `peer.frames`,
`native_pin_roots` and `native_pending_return`. Roots can only be added, so the
worst case is transient over-retention of at most 96 Strings per thread.

Both JIT guards are then removed from `vm/src/jit/skip_list.rs`, restoring JIT
coverage to the entire UnboundID SDK.

## Validation

All runs: real JDK 25, real sockets, the suite runner's own environment
(`CRATONVM_REAL_NET_SOCKETS` / `REAL_AQS` / `DISABLE_DEFAULT_WATCHDOG` /
`ROOTSNAP_CACHE`), `-Xmx2g`.

| Build | Config | Result | Reclaimed-live Strings / run |
| --- | --- | --- | --- |
| dev `e4e4053bb` (control, 2026-07-24) | guards lifted | **4 of 5 runs FAIL** (`Tests run: 76, Failures: 2` + CCE) | 3–8 |
| dev `95e4d9929` (pre-fix) | guards in place | 76/76 | 0 (guard hides it) |
| dev `95e4d9929` (pre-fix) | guards lifted | 76/76 × 38 runs | **3–4 (bug live, masked by the CP-class fallback)** |
| this branch, pre-merge | **guards removed**, no env overrides | 76/76 × 25 runs | **0** |
| this branch, after merging dev (+159 commits) | **guards removed**, no env overrides | 76/76 × 22 runs | **0** |
| dev `66ee9f037` on **Linux** (Azure host, `x86_64` 6.17.0-azure) | **guards removed**, no env overrides | 76/76 × 5 runs | **0** |

The Linux row matters because the root cause is a cross-thread GC-root gap, not
anything platform-specific, and because the 2026-07-24 Linux 6-shard run
(`apps/tomcat-suite-runner/RESULTS-20260724-cwdfix.md`) listed this class as
**HANG** among its 91 confirmed CratonVM-only regressions. It is no longer
hanging: 5/5 `OK (76 tests)` in 18-34s each, 0 stale-pointer events across all
five runs, against that host's own Tomcat fixture
(`/data/data/apps/tomcat`, `cp-linux-fixed.txt`, real JDK 25 at
`/home/victor/jdk25`).

The control build is the important row: the documented corruption still
reproduces on this box in this harness at the pre-fix commit, so "clean on
current dev" is a real fix and not a platform artifact.

`CRATONVM_DBG_JITC=1` confirms 562 UnboundID compile events per run with the
guards removed (566 after the dev merge) — including `RDN.getNameValuePairs`
(RDN.1's target) and `StaticUtils.toLowerCase` (the JIT.3 trigger) — so the
code really is compiled, C1 and C2, not merely admitted.

### Regression coverage

The fix only ever *adds* GC roots, but it is on a core path, so:

| Suite | Result |
| --- | --- |
| `cargo test -p cratonvm-vm --lib skip_list` | 64 passed / 0 failed pre-merge; 68 / 0 after the dev merge |
| Tomcat `catalina.realm.*` (11 classes) | 10 PASS + `TestJNDIRealm` FAIL — pre-existing fixture gap (no LDAP server on `127.0.0.1:12345`), identical on the pre-fix binary |
| Tomcat 36-class deterministic slice, A/B on the same tree | **class-for-class identical** before and after: PASS=29 FAIL=2 NOSUMMARY=4 HANG=1, 0 stale events in both |
| Spring Boot `spring-boot-micrometer-metrics` (82 classes) | 79 PASS / 3 EMPTY / 0 FAIL — the 2026-07-20 baseline for the same list was 76 PASS / 3 EMPTY / **3 FAIL**, and the same 3 EMPTY classes are base classes with no runnable tests |

> **Note for future stale-receiver hunts:** the 38 pre-fix runs all *passed*
> while emitting 3–4 stale-pointer warnings each. The interpreter's "falling
> back to CP class" recovery usually succeeds, so a green test is not evidence
> that the underlying use-after-free is gone. Count the warnings.
