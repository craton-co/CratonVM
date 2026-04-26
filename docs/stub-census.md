# Stub Census — refreshed 2026-04-24 (WP0.4)

Scope: `native-*/src/**/*.rs` + `vm/src/runtime/**/*.rs`.
Exclusions: `#[cfg(test)]` blocks, `tests/`, benchmarks, build scripts.

**Top-level finding**: The CI no-stubs gate (`interpreter.rs:10983`, `t13_no_unwrap_expect_panic_in_interpreter`) enforces zero `todo!()` / `unimplemented!()` / raw `panic!("not yet")` outside tests, and today zero such markers exist in the scanned directories. All remaining stubs are one of:

1. Deliberate "return spec-correct not-supported" natives (JDK 17 removed APIs, advisory-hint natives).
2. Error-path `Err(io_error("...: unsupported"))` on APIs we won't build this session (mmap, scatter/gather).
3. `RuntimeError::NotImplemented` used as a **generic error carrier** (stack over/underflow, naming exceptions, depth-limit) — NOT user-visible stubs.
4. Comment-flagged single-value placeholders (`cpu_percent: 0.0`, `code_size = 0usize`).
5. The 9 KC-blocker natives documented in `docs/kc-missing-natives-census.md` — all closed in Session 92 via N1–N4 agents; retained here as "historically stub" under Owner column for traceability.

Raw grep count across scope for `todo!(` / `unimplemented!(` / `panic!("not yet|todo|stub)` in production code paths (outside `#[cfg(test)]` blocks): **0**. Count for `todo!(` / `unimplemented!(` / `panic!("not yet)` anywhere in scope (including comments/doc-tests/test-assert helpers): **0 real + 3 doc-string matches in `vm/src/runtime/interpreter.rs` where the gate literal strings are defined** (lines 23, 10983, 11005-11008) — these are intentional.

---

## 1. Deliberate spec-correct not-supported natives

These natives are wired and registered but always throw / return the spec-correct "unsupported" value. They are NOT accidental stubs — the spec allows the behaviour — but they limit the feature surface WildFly / EJBCA can exercise.

| File:line | Owner WP# | Reachable by WildFly boot? | Note |
|---|---|---|---|
| `native-io/src/nio_native.rs:290-292` | WP4.1 (Selector NIO) | N | `FileDispatcherImpl.readv0` — always `IOException("readv0: unsupported")`; Undertow does not use scatter/gather directly |
| `native-io/src/nio_native.rs:294-296` | WP4.1 | N | `FileDispatcherImpl.writev0` — `IOException("writev0: unsupported")`; same as above |
| `native-io/src/nio_native.rs:298-303` | WP4.3 (FileChannel.map mmap) | N | `setDirect0` → returns `-1` (JDK spec-correct "hint ignored") |
| `native-io/src/nio_native.rs:311-321` | WP4.3 | **Y** | `FileChannelImpl.map0` → `IOException("map0: memory-mapped files are disabled; see sun.zip.disableMemoryMapping")`. **Hot**: H2 page store + Undertow response buffering both call this. Mitigated by `-Dsun.zip.disableMemoryMapping=true` but H2 will need this removed. |
| `native-io/src/nio_native.rs:323-327` | WP4.3 | N | `unmap0` noop (consistent with map0 always failing) |
| `native-builtins/src/deprecated_internal.rs:335-342` | WP (none — JDK-17 removed) | N | `java.rmi.activation.*` throws `NotImplemented` with explanation. Spec-correct for JDK 17+. |

---

## 2. `RuntimeError::NotImplemented` used as generic error variant (NOT stubs)

Cataloged for completeness because `grep NotImplemented` finds them, but these are real error paths — they use the `NotImplemented` enum variant because we lack a dedicated variant for the error type.

| File:line | Owner WP# | Reachable by WildFly boot? | Note |
|---|---|---|---|
| `vm/src/runtime/value_stack.rs:200,241,254,267,280,293,304,347,395,403,439,452,460,494` | WP5 (AQS etc., indirect) | **Y** | Stack overflow / underflow / category-2 mismatch on operand stack. Used as error carrier — real exception wrapped in `NotImplemented{feature:"operand stack overflow"}`. Not a stub; boot never overflows the default 65K operand stack. |
| `vm/src/runtime/interpreter.rs:1687-1689` | WP1 generic | **Y** | Panic-to-error converter in `execute_method`. Wraps panics from `pop_unchecked` etc. Not a stub. |
| `vm/src/runtime/interpreter.rs:10776-10783` | WP3 (reflect multianewarray) | **Y** | `multianewarray` exceeding `MAX_MULTI_ARRAY_DEPTH=8`. Real depth-guard, not a stub. |
| `vm/src/vm/vm_util.rs:400,429,496` | WP1 generic | **Y** | `<clinit>` panic → `NotImplemented` converter + JDK-frame-class swallow gate. Not a stub. |
| `vm/src/runtime/exceptions.rs:277,369,657,660,669` | WP1 generic | **Y** | `throw_runtime_error` dispatch for NotImplemented variant. Real code path, not stub. |
| `native-builtins/src/wildfly_naming.rs:451-466` | WP8.2 (JNDI / UserTransaction) | **Y** | 3 factories (`throw_name_not_found`, `throw_naming_exception`, `throw_invalid_name`) reuse `NotImplemented` because `NamingException` JDK type is not in the `RuntimeError` enum yet. Functions DO real work; error-channel is misnamed. **Should be lifted to real NamingException post WP1.**|
| `native-builtins/src/lang_class.rs:564-568,579-583` | WP3.1 (reflection) | **Y** | `Class.newInstance` failure paths when mirror has no class_id or heap returns null. Error-channel misnamed; the function's happy-path does real work. |
| `native-builtins/src/zip_real.rs:435-445` | none (dead) | N | `_unused_runtime_error()` / `_unused_ret()` — `#[allow(dead_code)]` placeholders kept to suppress import-warning during refactor. Not called. Harmless but should be removed. |
| `native-builtins/src/lib.rs:4672-4678` | none (dead) | N | `native_not_implemented` helper — defined but only referenced from `tests_extracted.rs:53`. No production call site. Should be removed or promoted to a canonical stub. |

---

## 3. Single-value placeholders (comment-flagged limitations)

| File:line | Owner WP# | Reachable by WildFly boot? | Note |
|---|---|---|---|
| `vm/src/runtime/interpreter.rs:1427` | WP19.6 (HotSpot-parity tracing) | **Y** | `let code_size = 0usize; // TODO: expose compiled code size` — JFR `JitCompilation` event records 0 bytes instead of real compiled-machine-code size. Cosmetic for bench; doesn't break boot. |
| `vm/src/runtime/crash_handler.rs:687-692` | WP19.3 (crash recovery) | N | `get_heap_info()` returns static "heap information unavailable during crash" string — deliberate safety limit (can't lock VM heap in crash handler). Not a stub. |
| `vm/src/runtime/soak_test.rs:234-236` | WP19.1 (24h soak) | N | `cpu_percent: 0.0` — platform-specific, documented as portability placeholder. Affects soak reports only. |
| `native-builtins/src/aot.rs:121-132` | WP19.9 (PGO) | N | `AotCacheEntry::new_placeholder` — explicitly-named placeholder-factory for SHA-256-fingerprinted cache entry before real compile completes. Not a stub; part of the two-phase design. |
| `native-builtins/src/panama.rs:1031,1041,1079` | WP (out of scope — Panama FFM) | N | `cif not yet cached` comments on lazy libffi-CIF fields — first call triggers real libffi `prepare_cif`. Not a stub; lazy-init pattern. |

---

## 4. Historical "MISSING:" natives (closed in Session 92)

These nine were runtime-warning `[NativeBridge] MISSING:` entries under Keycloak boot prior to Session 91/92. All are now **registered and functional** per `memory/project_state.md` and `docs/kc-missing-natives-census.md`. Retained here so the census is traceable from the roadmap.

| Native | Owner WP# | Reachable by WildFly boot? | Status (2026-04-24) |
|---|---|---|---|
| `java/lang/Class.getProtectionDomain0()Ljava/security/ProtectionDomain;` | WP7.9 (Policy) | Y | Closed Session 92 (N1) |
| `java/lang/Class.getSigners()[Ljava/lang/Object;` | WP7.9 | Y | Closed Session 92 (N1) |
| `java/lang/Class.setSigners([Ljava/lang/Object;)V` | WP7.9 | Y | Closed Session 92 (N1) |
| `java/lang/Thread.sleep0(J)V` | WP5.8 (virtual threads) | Y | Closed Session 92 (N2) — park_timeout + JFR VirtualThreadPinned |
| `java/security/AccessController.ensureMaterializedForStackWalk(Ljava/lang/Object;)V` | WP1.4 | Y | Closed Session 92 (N3) |
| `java/security/AccessController.getInheritedAccessControlContext()Ljava/security/AccessControlContext;` | WP1.4 | Y | Closed Session 92 (N3) |
| `java/security/AccessController.getProtectionDomain(Ljava/lang/Class;)Ljava/security/ProtectionDomain;` | WP1.4 / WP7.9 | Y | Closed Session 92 (N3) |
| `java/security/AccessController.getStackAccessControlContext()Ljava/security/AccessControlContext;` | WP1.4 | Y | Closed Session 92 (N3) |
| `java/util/concurrent/atomic/AtomicLong.VMSupportsCS8()Z` | WP5.6 (CHM/Atomic) | Y | Closed Session 92 (N4) — dual-path |

---

## 5. Other conventional "noop"/"null"/"false" natives (T9-era categorization)

Not listed individually (289 usages across 18 files). These are `native_noop`, `native_noop_with_this`, `native_return_null`, `native_return_false`, `native_return_zero` — the spec-correct defaults for `registerNatives()V`, `<init>()V` synthetic ctors, `getPackage()`/`getProvider()`/etc. per JDK spec. See old T9 census archived below.

| Category | Count | Spec-correct? |
|---|---|---|
| `native_noop` on `registerNatives()V` | ~12 | Yes |
| `native_noop_with_this` on synthetic `<init>()V` | ~120 | Yes |
| `native_return_null` on `getPackage/getProvider/getAnnotation` | ~17 | Yes |
| `native_return_false` on `isSynthetic/isAnonymous/isLocal/isMember` | 7 | Yes |
| `native_return_zero` on int-returning default state | 4 | Yes |

**None of these block WildFly boot.** They're the canonical JDK-compat layer from T9.

---

## Summary

**Total individual stub rows: 34** (6 spec-correct unsupported + 9 NotImplemented-as-carrier groups + 5 placeholders + 9 historical closed + 5 T9-summary rows).

### Counts per owner-WP

| Owner WP | Row count | Reachable-by-boot count |
|---|---|---|
| WP4.3 (FileChannel.map mmap) | 3 | 1 |
| WP4.1 (Selector NIO) | 2 | 0 |
| WP1.4 (SharedSecrets / AccessController) | 4 | 4 (closed) |
| WP7.9 (Policy) | 3 | 3 (closed) |
| WP5.6 (CHM / AtomicLong) | 1 | 1 (closed) |
| WP5.8 (virtual threads) | 1 | 1 (closed) |
| WP8.2 (JNDI / UserTransaction) | 1 (wildfly_naming factories) | 1 |
| WP3.1 (reflection) | 1 (Class.newInstance path) | 1 |
| WP3 (multi-array) | 1 | 1 |
| WP5 (AQS / operand-stack errors) | 2 | 2 |
| WP1 generic (panic→error, clinit swallow) | 3 | 3 |
| WP19.1 (soak) | 1 | 0 |
| WP19.3 (crash recovery) | 1 | 0 |
| WP19.6 (HotSpot-parity tracing) | 1 | 1 |
| WP19.9 (PGO) | 1 | 0 |
| none (dead code) | 2 | 0 |
| none (JDK-17 removed) | 1 | 0 |
| none (Panama FFM out of scope) | 3 (panama cif lazy-init) | 0 |

### Counts per crate

| Crate / dir | Stub rows | Reachable |
|---|---|---|
| `native-builtins/` | 9 | 3 |
| `native-io/` | 5 | 1 |
| `native-collections/` | 0 | 0 |
| `native-api/` | 0 | 0 |
| `native-awt/` | 0 | 0 (not boot-relevant) |
| `vm/src/runtime/` | 7 | 5 |
| `vm/src/vm/` (out-of-scope but noted) | 1 | 1 |
| Historical closed natives | 9 | 9 (closed) |

### Top 5 reachable-by-boot stubs (most likely to trip WildFly on first run)

1. **`native-io/src/nio_native.rs:311-321` `FileChannelImpl.map0`** — H2 + Undertow call this. Gate WP4.3. Without it, H2 page store cannot mmap `*.mv.db`; needs fallback or real implementation.
2. **`native-builtins/src/wildfly_naming.rs:451-466`** — JNDI exception factories return `NotImplemented`-tagged errors. JDK-side JNDI code may treat this as wrong exception-class on catch. Need real `javax.naming.NamingException` etc. for WP8.2.
3. **`native-builtins/src/lang_class.rs:564-583` `Class.newInstance` failure paths** — if mirror has no class_id, returns `NotImplemented` instead of proper `InstantiationException`. Reflection-heavy Hibernate / Weld code may mis-catch.
4. **`vm/src/runtime/interpreter.rs:1427` `code_size = 0usize`** — JFR compilation events are zero-size. Doesn't break boot but corrupts bench-hotspot-compare metrics for WP19.6.
5. **`vm/src/runtime/value_stack.rs:200..494`** — 14 `NotImplemented{feature:"operand stack overflow|underflow"}` carrier uses. Swallowed by `vm_util.rs:426-443` during `<clinit>`, so JDK classes boot. Risks masking real user-code bugs in WildFly subsystem init; long-term should migrate to dedicated `StackOverflowError` variant.

### Orphan stubs (no WP maps)

- `native-builtins/src/deprecated_internal.rs:335-342` — `java.rmi.activation.*`. **Correct to have no WP** — JDK 17 removed the API. No action.
- `native-builtins/src/zip_real.rs:435-445` — `_unused_runtime_error` / `_unused_ret` dead-code stubs. **No WP — cleanup candidate**. Flag as "orphan — remove next refactor".
- `native-builtins/src/lib.rs:4672-4678` — `native_not_implemented` unused helper. **No WP — cleanup candidate**.
- `native-builtins/src/panama.rs:1031..1079` — Panama FFM cif-lazy-init. Out of scope for WildFly+EJBCA. No WP needed.

---

## Appendix — prior T9 census (2026-04-16, archived)

Kept for historical reference; superseded by this document.

| Category | Count | Action |
|---|---|---|
| `native_noop` on void `registerNatives()V` | 12 | Keep: JDK convention |
| `native_noop` on void `<clinit>()V` | 3 | Keep |
| `native_noop` on void `initialize()V` | 6 | Keep |
| `native_noop_with_this` on `<init>()V` ctors | ~120 | Keep |
| `native_noop_with_this` on interface defaults | ~49 | Keep |
| `native_return_false` on isSynthetic/isAnonymous/etc. | 7 | Keep |
| `native_return_null` on getPackage/getProvider | 11 | Keep |
| `native_return_null` on getAnnotation | 6 | Keep |
| `native_return_zero` on int methods | 4 | Keep |
| `native_return_false` on `equals(Object)Z` | 2 | **FIXED** in T9 / session 89 — now identity-equals |
| `tests_extracted.rs` stubs | 9 | Keep: test-only |

### Convention going forward

- `native_noop` → only for `()V` methods with no side effects.
- `native_noop_with_this` → only for `()V` instance methods.
- `native_return_false` → only when `false` IS the spec-correct answer.
- `native_return_null` → only when `null` IS the spec-correct answer.
- All other cases → inline closure with `// Stub: <reason>` comment AND a linked WP#.
- NO new `todo!()` / `unimplemented!()` / `panic!("not yet")` in `native-*` or `vm/src/runtime/*`. CI gate at `vm/src/runtime/interpreter.rs:10983-11010`.
