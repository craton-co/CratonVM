# VM Core Review — `vm` crate core (`vm.rs` + `vm/` submodules)

**Reviewer:** Fable (Opus 4.8)
**Date:** 2026-06-10
**Scope:** `vm/src/vm.rs` (~68k LOC), `vm/src/vm/vm_exec.rs` (~10.9k), `vm/src/vm/vm_init.rs` (~9.3k), `vm/src/vm/vm_util.rs` (~2.8k), `vm/src/vm/vm_object.rs` (~1.3k); overall adequacy verdict on `vm/tests/` (91 files, ~30.7k LOC).
**Method:** static review only (no cargo). Grep for risk patterns, structural mapping, then deep reads of the highest-risk regions.

---

## Summary

The single most important structural fact: **`vm/src/vm.rs` contains no production code.** It is, from line 60 to the EOF at line 68246, one giant `#[cfg(all(test, feature = "synthetic-jdk"))] mod tests` block. All real VM logic lives in the four `vm/` submodules. The "~68k LOC giant" is therefore a synthetic-jdk-only unit-test file that does not even compile in the default (real-jdk) feature configuration. This dramatically lowers the risk surface of `vm.rs` itself (it is dead in production builds) but means its ~hundreds of tests provide **zero coverage in the default build**.

The production code (`vm_exec.rs`, `vm_init.rs`, `vm_util.rs`, `vm_object.rs`) is generally **defensively engineered and sound**:
- Every `unsafe` pointer copy in `vm_exec.rs` is preceded by explicit `checked_add` + length bounds checks (bulk array intrinsics, off-heap `copy_to/from_native_memory`).
- Native dispatch (`safe_native_call`) pins object args as GC roots and wraps the callback in `catch_unwind`.
- The class-init state machine (`ensure_class_initialized_shared` / `initialize_class_shared`) implements JVMS §5.5 correctly: TOCTOU-safe claiming under a write lock, recursive-init via thread-id, 30s wait timeouts, and `finalize_init` cleanup on both success and error paths.
- The blocked-thread GC fixup remaps every root category comprehensively.

The dominant *correctness/policy* concern is the **`<clinit>` error-swallowing path** (`vm_util.rs` `initialize_class_shared`) and its companion **`post_clinit_fixup` synthetic backfills**, which together violate JVMS §5.5 semantics for a broad framework allowlist and constitute the project's own forbidden "synthetic stubs that fake app behavior." There is a `CRATONVM_STRICT_SWALLOWS=1` escape hatch, but the default behavior masks real init failures.

No `unimplemented!`/`todo!` in production. No reachable panics from untrusted classfile input found (parsers use `.first().copied()`-style safe access; descriptor walks are bounds-checked).

---

## Bugs

### B1 (medium) — `<clinit>` failure silently swallowed → divergence from JVMS §5.5
`vm/src/vm/vm_util.rs:754-987`. When `<clinit>` throws, the code marks the class `Initialized` anyway for a large allowlist (`java/`, `jdk/`, `sun/`, `javax/`, `org/jboss/`, `io/quarkus/`, `org/wildfly/`, `io/smallrye/`, `org/springframework/boot/loader/`, `org/slf4j/impl/`, `ch/qos/logback/`, `com/sun/`) and a list of "swallowable" exception types (CCE, NPE, ISE, AIOOBE, NFE, `java/lang/Error`, etc.). A real JVM must transition the class to *Erroneous* and throw `ExceptionInInitializerError`/`NoClassDefFoundError` on every subsequent use. Here the class is left with null/synthetic statics. Consequence: latent NPEs surface far from the real cause; correctness silently diverges. Gated off by `CRATONVM_STRICT_SWALLOWS=1` but ON by default.

### B2 (medium) — `post_clinit_fixup` fabricates static state after a swallowed `<clinit>`
`vm/src/vm/vm_util.rs:1458-2289`. After B1 swallows a failure, this function backfills "critical" statics with synthetic objects: `LogManager.manager`, ICU `NormalizerBase$*ModeImpl.INSTANCE` wrapping a **pass-through `NoopNormalizer2`** (explicitly: "KC16 bootstrap never actually normalizes real Unicode, so the pass-through behavior is sufficient" — vm_util.rs:1525), VarHandle `FORM`, Quarkus `InitialConfigurator.DELAYED_HANDLER`, JBoss `DefaultBootModuleLoaderHolder.INSTANCE`, Spring `ApplicationStartup`, WildFly `ElytronMessages`, JBoss `ServiceLogger`. These are exactly the project-forbidden "synthetic stubs that fake app behavior." The NoopNormalizer2 is the clearest functional fake (silently produces wrong Unicode normalization). See Stubs section for the enumerated list.

### B3 (low) — class-loading flag not RAII-guarded; panic during load poisons the per-class mutex
`vm/src/vm/vm_init.rs:3004-3120` (`load_class_with_lock`-style path). `*loading` is set `true` at line 3005 and only reset to `false` at line 3110 after `cm.write().load_class(name)` returns. If `load_class` panics (the codebase has many `.expect`/`pop_unchecked` paths), the bool stays `true` and the `std::sync::Mutex` is poisoned, so every later loader of that class hits the `.expect("class-loading mutex poisoned…")` at line 2969 and itself panics — turning one class-load panic into a cascading abort. A `Drop`-guard that resets the flag and notifies waiters on unwind would contain it. Low severity because a panic during class loading already indicates a serious state, but it removes any chance of graceful degradation.

### B4 (low) — pointer-truncating coercion in `coerce_value_for_return`
`vm/src/vm/vm_exec.rs:111-122`. When a native/JIT return is type-confused (an `Object` value reaching an `I`/`J` return slot), the code does `Value::Int(p.as_ptr() as usize as i32)` (line 113) — truncating a 64-bit heap pointer to 32 bits. This is a defensive arm for a "shouldn't happen" case, but if it ever fires it silently produces a corrupted int rather than failing loudly. Prefer returning the default-zero (as the `b'F'`/`b'D'` arms effectively do) or asserting in debug.

### B5 (low) — dead but dangerous `value_as_object_ref`
`vm/src/vm/vm_exec.rs:304-311`. The *unvalidated* variant reinterprets any aligned `Value::Long` bits as an `ObjectRef` via `from_raw` — the exact pattern the surrounding comments call out as a GC-mark SEGV source. A repo-wide grep shows it currently has **no callers** in `vm/src` (everything uses `value_as_validated_object_ref`). It should be deleted or `#[cfg(test)]`-gated so a future caller can't reintroduce the hazard.

---

## Vulnerabilities

The VM by design executes untrusted bytecode and loads native libraries, so several "unsafe" surfaces are intentional. Findings below are about whether untrusted *input lengths/values* can break memory safety.

### V1 (low) — `load_native_library` loads and runs `JNI_OnLoad` from a user-controlled path
`vm/src/vm/vm_exec.rs:4453-4507`. `libloading::Library::new(&resolved)` + invoking the library's `JNI_OnLoad` is the standard JNI mechanism, but the path is resolved from `java.library.path` / `System.load`. This is inherent to JNI (HotSpot does the same), not a CratonVM-specific bug — flagged for the open-source threat model doc: untrusted classfiles that call `System.load("…")` get arbitrary native code execution exactly as on a real JVM. No additional sandbox. Acceptable for parity but worth documenting.

### V2 (low) — JNI native dispatch trusts descriptor-derived argument shape
`vm/src/vm/vm_exec.rs:9684-9768`. `dispatch_jni_native(fn_ptr, env, receiver, call_args, descriptor)` assembles the native call frame from the *descriptor string* (untrusted classfile data). If a malformed classfile registers a native with a descriptor that disagrees with the actual C ABI of the resolved symbol, this is UB inside the `unsafe` dispatch. This matches JNI semantics (the contract is the programmer's responsibility) but a length/arity sanity check before dispatch would harden it. The missing-native path correctly *warns* rather than crashing (line 9769-9777), which is good.

No memory-safety vulnerabilities found in the bulk-array or off-heap copy paths: all are bounds-checked up front (see B-section confirmations in Performance/notes).

---

## Stubs and Unimplemented

No `unimplemented!`/`todo!`/`NotImplemented`-returning natives exist in the production region of these files. The `RuntimeError::NotImplemented` usages in `vm_util.rs:651,769,868` are *error-classification* matches (catch a panic / classify a swallowed error), not stub returns.

The real stubs are the `post_clinit_fixup` synthetic-state fabrications (`vm/src/vm/vm_util.rs:1458-2289`). Each arm runs only after a swallowed `<clinit>`:

| Class fixed up | What it fabricates | vm_util.rs line |
|---|---|---|
| `java/util/logging/LogManager` | synthetic `manager` singleton | ~1505 |
| `jdk/internal/icu/text/NormalizerBase$NF{C,D,KC,KD,KC32}ModeImpl` | `INSTANCE` wrapping pass-through `NoopNormalizer2` (functionally fake normalization) | ~1513 |
| `java/lang/invoke/VarHandle{Ints,Longs,…}$Array` | `FORM` VarForm | ~1560 |
| `io/quarkus/bootstrap/logging/InitialConfigurator` | synthetic `DELAYED_HANDLER` | ~1608 |
| `org/jboss/modules/DefaultBootModuleLoaderHolder` | synthetic `LocalModuleLoader` `INSTANCE` | ~1638 |
| `java/math/BigInteger` | ZERO/ONE/TWO/TEN/NEGATIVE_ONE (this one is a legit JDK-layout patch, not app-faking) | ~1653 |
| `java/nio/file/attribute/PosixFilePermission` | 9 enum constants backfill | ~1786 |
| `java/math/BigDecimal` | ZERO/ONE/TWO/TEN (legit JDK-layout patch) | ~1943 |
| `org/springframework/core/metrics/ApplicationStartup` | `DEFAULT` instance | ~2061 |
| `org/jboss/msc/service/ServiceContainerImpl` | static backfill | ~2123 |
| `org/wildfly/security/auth/server/_private/ElytronMessages` | logger proxy backfill | ~2193 |
| `org/jboss/msc/service/ServiceLogger` | logger backfill | ~2240 |

The BigInteger/BigDecimal/PosixFilePermission arms are defensible (they repair a *known interpreter mis-resolution* of real-JDK static layout, not fake app behavior). The Quarkus/JBoss/Spring/WildFly/ICU arms are the policy-violating ones: they paper over capability gaps (resource loading, module loading, logging backend init) with empty/no-op singletons. Recommendation: each should be converted into a tracked gap and removed once the underlying capability lands (several arms in this same function were already removed with exactly that rationale — see the "(Removed)" comments at 1492-1504, 698-738).

---

## Performance

### P1 — `read_java_string` decodes byte[] strings element-by-element
`vm/src/vm/vm_object.rs:204-233`. The `Char` path uses `heap.read_char_array_bulk` (one memcpy), but the LATIN1 and UTF16 `Byte` paths loop calling `heap.get_array_element(value_array, i)` per byte — a virtual dispatch + `Value` box + element-type match per character. `read_java_string` is among the hottest functions in the VM (every map-key hash, `toString`, equality probe). A bulk byte read (mirroring `read_char_array_bulk`) would remove O(n) dispatch overhead per string read.

### P2 — `safe_native_call` pays `catch_unwind` + arg-pin loop on every native dispatch
`vm/src/vm/vm_exec.rs:384-438`. The `catch_unwind` landing-pad setup and the per-arg `pin_value_for_native_call` loop run on *every* native call (the central choke point). The debug instrumentation (ecwatch/memwatch/youngscan/ring) is correctly `OnceLock`/atomic-gated to near-zero when off, but the unwind guard and pin loop are unconditional. For native-heavy workloads this is real fixed overhead. Consider a fast path for natives proven panic-free / arg-free, or batching the pin push.

### P3 — `get_or_create_class_mirror` holds `class_mirrors.write()` across `class_manager.write()` + `load_class`
`vm/src/vm/vm_object.rs:377-405`. The slow path holds the mirror write-lock while taking the class-manager write-lock and calling `load_class("java/lang/Class")` (which itself acquires class-loading locks). This widens the critical section and serializes all mirror creation behind class loading. The `java/lang/Class` id + field count could be resolved *before* taking `class_mirrors.write()`.

### P4 — field-descriptor cache miss re-walks the full class hierarchy every time
`vm/src/vm/vm_exec.rs:784-791`. Misses are intentionally not cached (so a later synthetic→real promotion is picked up), meaning every `getfield`/`putfield` on a slot whose descriptor stays unresolvable re-runs the O(hierarchy-depth × fields) walk under a read lock. A negative-cache with invalidation on class promotion would avoid the repeated walk for the common "synthetic stub stays synthetic" case.

### P5 — debug env-var probes via `std::env::var(...).is_ok()` on warm paths
e.g. `vm/src/vm/vm_exec.rs:1782,1810`, `vm/src/vm/vm_object.rs:367-369`, `vm_util.rs:149,658`. Several `std::env::var("CRATONVM_DBG_*").is_ok()` calls sit on paths reached during normal execution (`new_ref_array`, `get_or_create_class_mirror`, `ensure_class_initialized` Module probe). `std::env::var` allocates + locks the process env table each call. The codebase already has the `OnceLock`-cached gate idiom (`youngscan_enabled`, env_cache); these stragglers should adopt it.

---

## Tests

### Adequacy verdict: ~55% estimated coverage of the production code in scope; does NOT plausibly reach 85%.

**Basis.**
- `vm/src/vm.rs`'s enormous inline test module is gated `#[cfg(all(test, feature = "synthetic-jdk"))]`. In the **default (real-jdk)** build it contributes *zero* coverage. So the headline "68k LOC of tests" is misleading for the shipped configuration.
- The production submodules' own `#[cfg(test)] mod tests` (vm_init.rs:4808-EOF ≈ half the file; vm_exec.rs:9886-EOF; vm_util.rs:2291-EOF; vm_object.rs:958-EOF) do exercise real paths: `create_real_jdk_vm`, `ensure_class_initialized`, Object method dispatch, vtable resolution, missing-native dumping. These are the strongest, most current tests and they target the right code.
- The 91 external `vm/tests/` files: 842 `#[test]` functions, of which **~102 (12%) are `#[ignore]`d** — many for legit env reasons (JAVA_HOME/PETCLINIC_JAR/javac on PATH) but several pin *known unfixed bugs* (CHM `transfer()` data-loss past 16 buckets; `Class.getDeclared{Methods,Fields,Constructors}` synthetic-jdk linkage gaps; `DefaultBootModuleLoaderHolder.loadModule` dispatch). **21 of 91 files (23%)** use `require_class_files!()`/`class_files_available()` which **silently `return` (pass)** when javac/.class files are absent — so an environment regression makes them quietly stop testing without any red signal.

**Which areas have tests vs none.**
- *Well covered:* String creation/decode, class-init ordering (`clinit_order_tests`, `management_factory_clinit`), exception edges, reflection surface (`wp2_*`), proxies/annotations, native bridge, interpreter opcodes (`interpreter_tests` — 169 tests, but env-gated), HotSpot differential harness (`differential.rs` — computes goldens live, so no staleness risk, but needs `java` on PATH).
- *Thin or none:* the `<clinit>` **swallow path and `post_clinit_fixup` arms** (B1/B2) — no test asserts that swallowing happens, that `CRATONVM_STRICT_SWALLOWS` escalates, or that a given fixup populates the right slot; the blocked-thread **GC fixup remap** (vm_exec.rs:909-974) — critical for correctness, no direct unit test in scope; **off-heap `copy_to/from_native_memory` arena routing** (vm_exec.rs:1692-1746); **`load_native_library`/JNI_OnLoad** path; concurrency of `ensure_class_initialized` under contention (there is `lock_order_smoke`/`monitor_stress` but not init-race-specific).

**Golden-value freshness.** The differential suite (`differential.rs`, `intrinsic_diff.rs`, `synthetic_diff.rs`) computes expected values by running real HotSpot at test time — so it is *structurally immune to golden staleness*, at the cost of requiring a JDK. The bintrees "golden 67674804" controversy noted in project memory lives in the bench harness, not these unit tests. No hardcoded stale checksums found in the in-scope test files.

**Most important missing tests (priority order):**
1. `<clinit>` swallow semantics: assert a failing framework `<clinit>` is swallowed-and-marked-Initialized by default, and that `CRATONVM_STRICT_SWALLOWS=1` converts it to an error. Pins B1's behavior so it can't silently widen.
2. `post_clinit_fixup` slot correctness: for BigInteger/BigDecimal/PosixFilePermission, assert the *named* statics land in the right static slots (the function has a documented history of off-by-static-index bugs).
3. Blocked-thread GC fixup: a test that relocates objects while a thread is blocked and asserts every root category (frames, locals, stack, `monitor_on_exit`, `native_pin_roots`, scoped values, pending exceptions) is remapped.
4. Off-heap copy bounds: `copy_to/from_native_memory` and the bulk-array intrinsics with `dst_off + len` overflow and out-of-bounds inputs returning `false`/`0` rather than writing.
5. `ensure_class_initialized` contention: two threads racing to initialize the same class; assert exactly one runs `<clinit>` and the other blocks then observes Initialized.

---

## Feature Suggestions

1. **Strict-conformance mode by default for open-source.** Invert the swallow default: make `<clinit>` failures fatal (JVMS-correct) and require an opt-in `CRATONVM_LENIENT_BOOT=1` for the gauntlet. The current default ships JVMS-divergent behavior, which is a poor first impression for an Apache-2.0 JVM and a footgun for users who run their own classes.
2. **Promote `post_clinit_fixup` arms to a tracked-gap registry.** Replace the inline app-specific backfills with a single table + a `docs/gaps/` entry per arm, and emit a one-line `tracing::warn!` with the gap id when a fixup fires, so users can see exactly which capability is being faked.
3. **Bulk byte[]-string decode.** Add `VmHeap::read_byte_array_bulk` and route `read_java_string`'s LATIN1/UTF16 paths through it (P1) — likely a measurable interpreter-wide win.
4. **RAII class-loading guard.** Wrap the `*loading = true` … `*loading = false` span (B3) in a drop guard that resets the flag and `notify_all()`s on unwind, so a class-load panic degrades to a per-class error instead of a poisoned-mutex cascade.
5. **Native-dispatch descriptor/arity validation.** Before `dispatch_jni_native` (V2), validate that the parsed descriptor arity matches `call_args.len()` and reject mismatches with `UnsatisfiedLinkError` instead of entering the `unsafe` call with a malformed frame.
6. **Coverage gating in CI.** Fail the build if javac is absent (so the 21 silent-skip files can't quietly no-op), or at minimum print a prominent summary of how many tests were skipped for missing prerequisites.

---

## Files sampled vs fully read

**Production code:**
- `vm/src/vm.rs` — structurally mapped (confirmed entirely `#[cfg(all(test, feature="synthetic-jdk"))] mod tests`, lines 60–68246; no production code). Sampled head + tail + grep of impl/struct/panic; not deep-read line-by-line (it is test code, out of the production risk surface).
- `vm/src/vm/vm_exec.rs` — deep-read the high-risk regions: return-coercion + object-ref extraction (109-334), `safe_native_call` + gates (344-609), field-descriptor cache (635-792), off-heap/bulk-array unsafe (1692-2160), blocked-thread GC fixup (905-974), `load_native_library` (4453-4507), JNI dispatch (9680-9777). Grepped the giant `invoke_on_class_shared_inner` (7448-9863) for unchecked indexing/unwrap (none found); not line-by-line.
- `vm/src/vm/vm_init.rs` — deep-read bootstrap class loading (800-894), class-loading lock/condvar + CHA invalidation (2940-3160), finalizer/cleaner/reference plumbing (3123-3178). Grepped all production panics/expects (816-3865); test module (4808-EOF) sampled only.
- `vm/src/vm/vm_util.rs` — fully read the production half (1-2289): `ensure_class_initialized_shared` (118-283), `initialize_class_shared` (374-560), the `<clinit>` run + swallow path (560-1010), `post_clinit_fixup` (1458-1690 in detail, 1690-2289 enumerated).
- `vm/src/vm/vm_object.rs` — fully read the production half (1-956): string decode/read (190-333), class-mirror cache (366-485), static get/set, pre-init helpers, native coverage reporting.

**Tests:** inventoried all 91 `vm/tests/` files (sizes, ignore counts, skip-guard usage). Deep-read `interpreter_tests.rs` harness (1-52) and `differential.rs` harness (1-280); structurally surveyed `tier1_tests.rs`, the `wp2_*` reflection cluster, and the ignore/skip distribution. Did not read every test body.
