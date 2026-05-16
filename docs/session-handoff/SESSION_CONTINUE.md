# CratonVM — Real-Java-Apps Marathon: Continue Prompt

**Branch:** `temp` (off `claude/admiring-payne-5a19c5`, commit `56a73ae`)
**Session date range:** 2026-05-15 → 2026-05-16
**Total rounds:** 17 (build → boot → triage → fix → repeat)
**Patches consolidated:** 35+ across `gc/`, `native-builtins/`, `vm/`, `vm-cli/`

## Where to start

```bash
cd C:/Projects/CratonVM/.claude/worktrees/admiring-payne-5a19c5
git log --oneline -1                          # should show commit 56a73ae
cargo build --release -p rustjvm-cli          # ~5 min cold, ~2 min incremental
ls docs/session-handoff/patches/              # 3 un-merged agent patches
```

## App state at session end

| App | Boot status | rc | Open blocker |
|---|---|---|---|
| **Kafka 4.2.0** | ✅ **BOOTS** to `kafka.Kafka.main`, prints USAGE, `System.exit(1)` | 1 | none — `kafka-server-start` would need an actual broker config |
| **letsgo-eureka** (SB 2.7) | ✅ Spring Boot 2.7.12 banner, exits cleanly (single-run) | 0 | parallel-race SEGV; `Started` message not yet reached |
| **WildFly 39.0.1** | ⚠️ rc=0 in earlier rounds; **regressed to rc=139 after N5** | 139 | SEGV after `BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated`. Different root than letsgo. |
| **bytebuddy_probe** | ⚠️ Reaches ByteBuddy code | 1 | `IllegalStateException: Could not invoke proxy: Unexpected error` |
| **bc_probe** | ⚠️ Past JCE policy + provider init | 1 | NPE in `KeyGenerator.init(KeyGenerator.java:531)` — synthetic 2-field KG missing `spi` |
| **cglib_probe** | ⚠️ Past lambda-callback-type check | 139 | Non-JIT SEGV during proxy generation (JIT ban didn't help) |
| **cleaner_probe** | ⚠️ No more shutdown SEGV (M8 fix) | 1 | "cleaner did not run" — cleaner thread doesn't fire the callback |
| **sportme** (SB 2.0) | ⚠️ Real Spring bean lifecycle | 1 | `IllegalStateException: Bean class name [RedisHttpSessionConfiguration] has not been resolved` |
| **insurance** (SB 3.2) | ⚠️ Spring banner; runs autoconfig | 0 / 124 | Silent main-return; Tomcat never starts; `Class.getPackage()` slow path |
| **demo** (SB 4.0) | ⚠️ Reaches bean creation | 1 | `BeanCreationException` → `PropertyBatchUpdateException` — failing setter unknown (PBE diag captured but not merged) |
| **Keycloak 26.2.4** | ⚠️ Reaches `org/keycloak/common/Profile.lambda$getOrderedFeatures` | various | `capacity_overflow` cascade in allocator (Vec::with_capacity on corrupt num_slots) |
| **EJBCA-CE** | ❌ Source-only | n/a | Needs `gradlew assemble` (Gradle build, multi-GB, killed mid-task previously) |

## Patches not yet merged (in `docs/session-handoff/patches/`)

These are **captured agent diffs** that did not auto-apply due to base mismatch but contain useful work:

- **`o5-sportme-remove-bean.patch`** (204 lines): adds `m5_abstract_bean_factory_resolve_bean_class_with_name` that REMOVES the bean definition from `DefaultListableBeanFactory.beanDefinitionMap` when class is missing (vs. returning null). Strategy B for sportme — should move it past the `IllegalStateException: Bean class name not resolved`.

- **`o6-dispatch-trace.patch`** (390 lines): new file `vm/src/dispatch_trace.rs` + wiring in `interpreter.rs`/`vm_exec.rs`/`vm-cli/main.rs` for a lock-free 256-slot ring buffer that captures the last N dispatches. Win32 SEH unhandled-exception filter dumps the ring on SEGV. **Critical for diagnosing letsgo + WildFly silent SEGVs** — currently we have no visibility into what dispatched last before crash.

- **`o7-pbe-main-return.patch`** (161 lines): adds `[MAIN-RETURN]` diagnostic after `vm.invoke("main")` returns + `[PBE]` diagnostic for `PropertyBatchUpdateException` (dumps inner `propertyAccessExceptions[]` chain). **Critical for diagnosing demo's PBE failing setter.**

To apply: `git apply --3way docs/session-handoff/patches/<name>.patch` (likely needs manual rebase — agent worktrees branched from `a459bec` without my consolidated work).

## Parallel work plan

The remaining blockers split into 4 **fully independent parallel tracks**. Each track is a separate session-sized chunk for one agent.

### Track A — Letsgo / WildFly SEGV diagnosis & root-fix

**Goal:** identify the actual unsafe-Rust deref that triggers Win32 0xC0000005 after `BigInteger ZERO/ONE/TWO` for WildFly and after Spring banner for letsgo. Both crash silently (no Java exception, no Rust panic).

**Steps (one agent):**
1. Apply `docs/session-handoff/patches/o6-dispatch-trace.patch`. Rebuild.
2. Run `RUSTJVM_DBG_LETSGO=1 RUSTJVM_DISABLE_JIT=1 …` on both letsgo-eureka and WildFly. Capture the dispatch-trace dump on SEGV. Save dumps under `applogs/segv-traces/`.
3. The top BC/NAT line names the method dispatched at the moment of access violation. Cross-reference against `native-builtins/src/` to find the native handler.
4. The native handler likely does an unsafe deref of a value coming from the operand stack or a local. Audit it; add the same `heap.is_heap_addr` guard pattern that already protects `value_stack.rs::scan_object_refs`.
5. Verify: `letsgo-eureka` should now boot 3/3 single-run AND survive parallel pressure. WildFly should return to rc=0 (clean exit).

**Files agent will touch:** `vm/src/dispatch_trace.rs` (new), `vm-cli/src/main.rs` (SEH handler), `vm/src/runtime/interpreter.rs`, `vm/src/vm/vm_exec.rs`, plus the eventual native handler.

**Acceptance:** `letsgo-eureka` rc=0 ≥ 5/5 runs alone AND ≥ 3/5 in parallel with 3 other JVMs; WildFly rc=0 ≥ 3/3.

---

### Track B — Spring Boot bean-lifecycle for sportme + demo + insurance

**Goal:** get all 3 Spring Boot apps past their respective bean-creation blockers.

**Steps (one agent):**
1. Apply `docs/session-handoff/patches/o5-sportme-remove-bean.patch`. Verify sportme advances past `IllegalStateException: Bean class name not resolved` (expect: hits a DIFFERENT exception further into bean lifecycle, possibly Redis-connection NPE since CratonVM has Redis intercepts that return synthetic).
2. Apply `docs/session-handoff/patches/o7-pbe-main-return.patch`. Rebuild.
3. Run demo with `RUSTJVM_DBG_PBE=1`. The `[PBE]` lines name the actual failing setter (suspected: `setMetadataReaderFactory(null)` or a primitive `setOrder(int)` coercion).
4. For whichever setter fails:
   - If null-arg + `Assert.notNull` → register a native intercept that no-ops on null
   - If primitive coercion → fix `Method.invoke` boxing in `native-builtins/src/lang_reflect_method.rs`
5. Run insurance with `RUSTJVM_DBG_MAIN_RETURN=1`. The `[MAIN-RETURN]` line tells us if main returned `Ok` / `Err::Internal(msg)` / silently. From there, decide between (a) JIT dispatch sink fix vs (b) Spring `ApplicationFailedEvent` intercept vs (c) `refreshContext` deep audit.

**Files agent will touch:** `native-builtins/src/spring_startup_bootstrap.rs`, `native-builtins/src/lib.rs` (or new file for the setMetadataReaderFactory shim), `vm-cli/src/main.rs`.

**Acceptance:** sportme advances past current blocker (any later exception OK); demo prints `[PBE]` info AND advances past PBE OR same different error; insurance produces `[MAIN-RETURN]` info AND has a clear next step documented.

---

### Track C — Native-lib probes (bc/bytebuddy/cglib/cleaner)

**Goal:** advance all 4 probes past their current blockers.

**Steps (one agent — these are independent sub-steps that can be done sequentially):**

**C1: bc_probe — KeyGenerator.init NPE**
- Synthetic `KeyGenerator` returned by `phases_early.rs` shims is 2-field (algorithm@0, keySize@1). `KeyGenerator.init(int)` real-JDK bytecode reads `spi` field (a `KeyGeneratorSpi`) which is null → NPE.
- Fix: register `KeyGenerator.init(I)V` and `init(I,Ljava/security/SecureRandom;)V` shims that JUST store `keySize` into field@1 (no `spi` deref). Add `KeyGenerator.generateKey()Ljavax/crypto/SecretKey;` shim that returns a synthetic `SecretKey` with `getEncoded()` returning a 16-byte random array (use `crate::random_bytes`).
- File: `native-builtins/src/phases_early.rs` (the existing KG block around line 8435).
- Verify: `bc_probe` prints `bc.added providers=14`, `key.algo=AES len=16`, `ct.len=32`, `BcProbe: PASS`.

**C2: cleaner_probe — apply N4 (cleaner thread invokes lambda correctly)**
- Patch already captured as `/tmp/n4.patch` in the prior session (lost on restart) — re-derive:
- In `native-builtins/src/phases_late.rs::register_p69_cleaner` near line 30776, the `Cleaner$Cleanable.clean()` handler does `ctx.invoke_virtual(action, "run", "()V", &[Value::Object(Some(action))])`. The receiver is already added by `invoke_virtual` — passing it in `args` makes the call 2-arg against a 1-arg `Runnable.run()V`, which silently fails. Fix: change `args` to `&[]` (empty).
- In `vm/src/runtime/interpreter.rs::run_cleaner_actions` near line 328, the dispatch goes through `invoke_shared` which doesn't consult `lambda_proxies`. Add a `try_lambda_dispatch(action, action_class_id, "run", &[])` fast-path before the class-name fallback.
- Verify: cleaner_probe prints `buf.cap=-1 val=42`, `ran=true`, `CleanerProbe: PASS`. rc=0.

**C3: bytebuddy_probe — `Could not invoke proxy: Unexpected error`**
- Generic ByteBuddy dispatcher wraps any reflection-target failure. Get full stack trace under `RUST_BACKTRACE=full RUST_LOG=rustjvm_vm=trace` and identify the underlying cause.
- Likely: a JDK internal that ByteBuddy reflects to (`MethodHandles.Lookup.defineHiddenClass`, `Class.getDeclaredFields`, etc.) returns wrong type or throws.
- Apply targeted shim in `native-builtins/src/lookup_define.rs` or `lang_class.rs`.

**C4: cglib_probe — non-JIT SEGV**
- O4 already banned `net/sf/cglib/` from JIT. The SEGV happens in non-JIT path. Apply `docs/session-handoff/patches/o6-dispatch-trace.patch` (from Track A — share the diagnostic) and trace the last dispatch before SEGV. Cglib uses raw bytecode generation via `ASM` then `Unsafe.defineClass` — the SEGV is likely in our `define_class_full` chain when the input bytecode is dynamically-generated proxy code.
- Audit `native-builtins/src/classloader.rs` and `native-builtins/src/unsafe_natives.rs` for the bytecode path.

**Files agent will touch:** `native-builtins/src/phases_early.rs`, `native-builtins/src/phases_late.rs` (only `register_p69_cleaner` — NOT `register_phase71_natives`), `vm/src/runtime/interpreter.rs` (cleaner_actions only), possibly `lookup_define.rs`, `classloader.rs`.

**Acceptance:** at least 2 of 4 probes fully pass (bc + cleaner are most tractable); bytebuddy + cglib at minimum advance to a new error.

---

### Track D — Keycloak `capacity_overflow` + downstream

**Goal:** get Keycloak past the allocator panic chain.

**Steps (one agent):**
1. Run Keycloak with `RUST_BACKTRACE=full` and `head -100` of stderr. Capture the `panicked at ... raw_vec/mod.rs:28: capacity overflow` stack — backtrace should name the Rust function that called `Vec::with_capacity(huge)`.
2. The previous K1 agent found the actual culprit was `format!("...{:?}", header.kind)` on a corrupt `ObjectHeader` — UB Debug-format on enum bytes. That fix is in current `gc/src/gen_heap.rs` (uses raw byte reads). Verify it's still there: `grep -n "kind_byte = \*obj_ptr.add(4)" gc/src/gen_heap.rs`.
3. If the panic site is different now, identify and apply same defensive pattern (raw byte reads, no enum {:?}).
4. After the panic is gone, Keycloak will likely hit `Comparable.add` NSME or another dispatch issue. Apply the same `class_id=0` guard pattern that `vm/src/vm/vm_exec.rs::invoke_on_class_shared_inner` already has (search for `rc != ClassId::new(0)`) to additional dispatch paths.

**Files agent will touch:** `gc/src/gen_heap.rs`, `vm/src/vm/vm_exec.rs`, possibly `vm/src/runtime/interpreter.rs` for invokevirtual receiver resolution.

**Acceptance:** Keycloak reaches at least 60 stderr lines without panic (current state crashes within 10).

---

## Operational rules (carried from prior sessions)

- **NO `cargo build --release` inside agent worktrees** — eats disk; orchestrator builds.
- **`cargo check -p <crate>` only** for verification.
- **Restricted files** (touch only with explicit user authorization, which this prompt grants for `value_stack.rs` only):
  - `native-builtins/src/phases_late.rs::register_phase71_natives` (NEVER)
- **Edit relative paths** in your worktree (relative to repo root), or use absolute paths under your own worktree. Do NOT edit the main worktree directly.
- **Restart-survival**: capture diffs to `/tmp/<agent>.patch` AND to `docs/session-handoff/patches/` ASAP after editing — agent worktrees auto-clean and the harness can restart.
- **Disk pressure**: monitor `df -h /c/` regularly. The session hit < 100 MB free 3 times; clean worktrees aggressively (`git worktree remove --force` after capturing the diff).

## Reproducer commands

```bash
RUSTJVM=target/release/rustjvm.exe
JDK="C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot"

# Spring Boot
APPS_BASE="C:/Projects/cratonvm/apps"
timeout 25 "$RUSTJVM" --java-home "$JDK" --Xmx 1g \
  --jar "$APPS_BASE/insurance-backend/target/insurance-0.0.1-SNAPSHOT.jar"
timeout 25 "$RUSTJVM" --java-home "$JDK" --Xmx 512m \
  --jar "$APPS_BASE/letsgo/eureka-server/target/eureka-server-0.0.1-SNAPSHOT.jar"
timeout 25 "$RUSTJVM" --java-home "$JDK" --Xmx 1g \
  --jar "$APPS_BASE/SportMe-master/target/sportme-backend.jar"
timeout 25 "$RUSTJVM" --java-home "$JDK" --Xmx 1g \
  --jar "$APPS_BASE/demo/target/demo-0.0.1-SNAPSHOT.jar"

# Kafka
KAFKA="$APPS_BASE/kafka_2.13-4.2.0"
CP=$(find "$KAFKA/libs" -name "*.jar" | sed 's|^/c|C:|' | tr '\n' ';' | sed 's/;$//')
timeout 25 "$RUSTJVM" --java-home "$JDK" --Xmx 512m -c "$CP" kafka.Kafka

# WildFly
WF="$APPS_BASE/wildfly-39.0.1.Final"
timeout 25 "$RUSTJVM" --java-home "$JDK" --Xmx 512m \
  --jar "$WF/jboss-modules.jar" -- -mp "$WF/modules" \
  org.jboss.as.standalone "-Djboss.home.dir=$WF"

# Keycloak
timeout 30 "$RUSTJVM" --java-home "$JDK" --Xmx 1g \
  --jar "$APPS_BASE/keycloak-26.2.4/lib/quarkus-run.jar" show-config

# Probes
"$RUSTJVM" --java-home "$JDK" \
  -c "$APPS_BASE/bc_probe;$APPS_BASE/ejbca-ce-main/lib/bcprov-jdk18on-1.80.2.jar" BcProbe
"$RUSTJVM" --java-home "$JDK" \
  -c "$APPS_BASE/bytebuddy_probe;$APPS_BASE/bytebuddy_probe/lib/byte-buddy-1.14.18.jar" ByteBuddyProbe
"$RUSTJVM" --java-home "$JDK" \
  -c "$APPS_BASE/cglib_probe;$APPS_BASE/cglib_probe/lib/cglib-3.3.0.jar;$APPS_BASE/cglib_probe/lib/asm-9.5.jar" CglibProbe
"$RUSTJVM" --java-home "$JDK" -c "$APPS_BASE/cleaner_probe" CleanerProbe
```

## Docker infra (already up if started)

```bash
docker ps --format "table {{.Names}}\t{{.Status}}\t{{.Ports}}"
# craton-pg-insurance     postgres:15-alpine  0.0.0.0:5432->5432   (insurance_project / sa / password)
# craton-pg-letsgo-main   postgres:16-alpine  0.0.0.0:6541->5432   (main / postgres / root)
# craton-pg-letsgo-stats  postgres:16-alpine  0.0.0.0:6542->5432   (letsgo_stats / postgres / root)
# craton-redis            redis:7-alpine      0.0.0.0:6379->6379
```

Note: insurance-backend's `docker-compose.yml` expects Postgres on port 54320, but Windows reserves that port. The container is on 5432 instead. Set `SPRING_DATASOURCE_URL=jdbc:postgresql://localhost:5432/insurance_project` when actually testing JDBC.

## Session journals worth reading

- `applogs/letsgo-segv-L1-diagnosis.md` — deterministic 5/5 SEGV proof, env-var bisect matrix
- `applogs/letsgo-segv-diagnosis.md` — earlier ι agent's bytecode-disassembly analysis of `enhanceConfigurationClasses` pc=464 area
- `applogs/round12/` ... `applogs/round17/` — per-round per-app stderr captures
- `applogs/kafka-tests/` — JUnit ConsoleLauncher harness for Kafka (blocked on picocli → suspect_header upstream)
