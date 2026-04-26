# Keycloak 16 / Keycloak 26 Missing-Natives Census

**Session 87 (2026-04-24)**. Sibling agents N1..N4 are fixing the 9 known static-analysis natives in parallel with this census.

## Scope

Enumerate every `UnsatisfiedLinkError` surface that appears during a KC16 (jboss-modules / WildFly) and KC26 (Quarkus 3.20 fast-jar) boot on RustJVM against JDK 21.0.6, both the 9 startup-warning entries and any deeper-surfacing natives that only appear during a specific classloader / reflection / Jackson / Hibernate code path.

## Capture procedure

Both targets were launched with the NEW-10 CLI flags:

```
rustjvm.exe --java-home "C:\craton\jdk-21.0.6" --Xmx 1g \
    --dump-missing-natives <path>.json \
    --dump-missing-natives-grouped <path>_grouped.json \
    --XX:AuditMissingNatives \
    --jar <entry.jar> [<args>]
```

For KC16: `--jar jboss-modules.jar -- -mp modules -jaxpmodule javax.xml.jaxp-provider org.jboss.as.standalone -b 0.0.0.0`.
For KC26: `--jar lib/quarkus-run.jar`.

## Capture limitation (Session 87)

The JSON dump is written on **natural** Rust `main()` return (see `vm-cli/src/main.rs` line 691 and 708). There is **no SIGTERM / Ctrl-Break handler** registered — `Windows TerminateProcess` and `kill -9` bypass the shutdown code entirely, so a hung KC boot cannot be reaped without losing the audit log.

On Session 87 both KC targets reached the JDK-21 bootstrap classloader phase and then **stopped making progress**:

- **KC26**: `kc26_d12_stderr.txt` (487 KB, 2160 lines) shows 377 classes loaded, the last being `java/lang/Character$CharacterCache` / `java/util/concurrent/atomic/AtomicInteger.<clinit>`. The process then goes silent and never exits.
- **KC16**: `kc16_stderr.txt` (line 12, historical at 2026-04-24T00:47:03Z) records one dynamic `Missing native method in real-JDK mode method=java/lang/StringUTF16.isBigEndian()Z` followed by a `B6: silent-swallow ... class=java/lang/StringUTF16 exc=java/lang/UnsatisfiedLinkError`. Current KC16 runs stall at the same point without emitting further entries.

Across eight attempts (timeout 180s, SIGTERM, SIGINT, PowerShell `CloseMainWindow()` then `Kill`, `taskkill -F`) the dump file was **not produced** for either target on Session 87. The census below is therefore sourced from:

1. The deterministic 9-entry `[NativeBridge] 9 unregistered native methods` startup warning produced by every boot (this is a static bridge scan, not an invocation log).
2. One historical dynamic finding from an earlier session that has since been addressed.

## Totals

| metric | KC16 | KC26 | shared |
|---|---|---|---|
| total missing natives | 9 | 9 | 9 |
| blocking (would throw UnsatisfiedLinkError, not B6-swallowed) | 2 | 2 | 2 |
| non-blocking (static warning only, or B6-swallow candidate) | 7 | 7 | 7 |

All 9 natives are identical across KC16 and KC26 because both VMs use the same JDK 21.0.6 bootstrap classpath. The shared intersection = 9.

## Per-module breakdown

| JDK module | count | entries |
|---|---|---|
| `java.base` | 9 | `java/lang/Class.{getProtectionDomain0, getSigners, setSigners}`, `java/lang/Thread.sleep0`, `java/security/AccessController.{ensureMaterializedForStackWalk, getInheritedAccessControlContext, getProtectionDomain, getStackAccessControlContext}`, `java/util/concurrent/atomic/AtomicLong.VMSupportsCS8` |
| `java.management` | 0 | — |
| `java.security.sasl` | 0 | — |
| `java.sql` | 0 | — |
| `jdk.unsupported` | 0 | — |
| `sun.*` / `jdk.internal.*` | 0 | — |

Every entry lands in `java.base`. No deeper-module natives (`java.net.http`, `java.sql`, `jdk.management.jfr`, `jdk.unsupported`) surfaced because the boot never advances that far on Session 87.

## Top 10 most-frequently-called missing natives

**Not available.** The `MissingNativeEntry` struct at `vm/src/vm/vm_init.rs:44` stores only `class_name`, `method_name`, `descriptor`, and `sample_call_site`; there is no `call_count` field, and `SharedVm::record_missing_native` (line 1075) deduplicates by `(class, name, descriptor)` before pushing, so each signature appears at most once in the log.

If call-frequency is needed, the `MissingNativeEntry` struct and the `record_missing_native` dedupe should be extended (out of scope for this census).

## Blocking vs non-blocking

- **Blocking (2)**:
  - `java/util/concurrent/atomic/AtomicLong.VMSupportsCS8()Z` — gates the entire `java.util.concurrent.atomic` tree. `AtomicInteger`, `AtomicReference`, `AtomicLongFieldUpdater`, `ConcurrentHashMap`, `StampedLock`, `Phaser`, `ForkJoinTask` counters, `CompletableFuture`, `ThreadPoolExecutor.ctl` all depend on atomic long CAS. Today this throws `UnsatisfiedLinkError` when called and is **the most likely cause of the KC26 boot stall at `AtomicInteger.<clinit>`**.
  - `java/lang/Thread.sleep0(J)V` — called by Narayana, Infinispan, Quarkus runtime retry loops and park-with-timeout code paths. A no-op would spin the CPU; a real stub must `park_timeout(nanos)`.

- **Non-blocking (7)**:
  - `java/lang/Class.{getProtectionDomain0, getSigners, setSigners}` — ProtectionDomain/Signers are `null` / `[]` / no-op in a VM without SecurityManager.
  - `java/security/AccessController.*` — all 4 return `null` or are no-ops when no SecurityManager is installed (the default on modern JDKs).

## Historical dynamic finding (resolved)

`java/lang/StringUTF16.isBigEndian()Z` — surfaced in `/tmp/kc16_stderr.txt` line 12 on a KC16 boot at 2026-04-24T00:47:03Z. Addressed during Session 87: `native-builtins/src/lang_string.rs:3011` defines `native_string_utf16_is_big_endian` returning a `target_endian` compile-time constant, wired via `register_string_utf16_natives` (line 3022 + `lib.rs:2073`). Not resurfacing in current captures.

## Cross-check vs pre-existing baseline

- `bench/missing-natives.json` = `{"missing_natives": []}` (empty — HelloWorld has no missing natives).
- `bench/missing-natives-grouped.json` = `{"version":1,"modules":{}}` (empty).

Neither baseline contains any entries, so there is nothing to flag as stale or missing from the live capture. This KC census is the first populated delta — committed as `bench/missing-natives-kc.json`.

## Recommended fix order

| rank | native | severity | assigned |
|---|---|---|---|
| 1 | `java/util/concurrent/atomic/AtomicLong.VMSupportsCS8()Z` | **blocking**, gates all atomic CAS | Agent N4 |
| 2 | `java/lang/Thread.sleep0(J)V` | **blocking**, KC/Narayana/Quarkus retry loops | Agent N2 |
| 3 | `java/security/AccessController.getStackAccessControlContext()Ljava/security/AccessControlContext;` | non-blocking, pervasive via `doPrivileged` | Agent N3 |
| 4 | `java/security/AccessController.getInheritedAccessControlContext()Ljava/security/AccessControlContext;` | non-blocking, `Thread.<init>` inherit-ACC path | Agent N3 |
| 5 | `java/security/AccessController.getProtectionDomain(Ljava/lang/Class;)Ljava/security/ProtectionDomain;` | non-blocking | Agent N3 |
| 6 | `java/security/AccessController.ensureMaterializedForStackWalk(Ljava/lang/Object;)V` | non-blocking, `StackWalker` opt-in | Agent N3 |
| 7 | `java/lang/Class.getProtectionDomain0()Ljava/security/ProtectionDomain;` | non-blocking | Agent N1 |
| 8 | `java/lang/Class.getSigners()[Ljava/lang/Object;` | non-blocking | Agent N1 |
| 9 | `java/lang/Class.setSigners([Ljava/lang/Object;)V` | non-blocking | Agent N1 |

## Follow-ups

1. **Re-run this census** after N1..N4 land. Expect a deeper wave from `jdk.internal.misc.Unsafe.*` fences, `java.lang.ref.Reference.*`, `MethodHandleNatives.*` resolve, Jackson/Hibernate reflection, and Quarkus classloader intrinsics — none of which surface today because the boot stops at `AtomicInteger.<clinit>`.
2. **Add a graceful-shutdown mechanism** to RustJVM so hung boots still dump the audit log. Options:
   - `--shutdown-after-seconds N` CLI flag that spawns a timer thread and flushes on timeout.
   - A `ctrlc`-crate SIGINT/SIGTERM handler that calls `SharedVm::dump_missing_natives_json` before `std::process::exit`.
   This would immediately unlock real dynamic-audit data for every blocking boot scenario.
3. **Investigate the KC26 stall at `AtomicInteger.<clinit>`**. The likely cause is that `AtomicLong.VMSupportsCS8` throws `UnsatisfiedLinkError` which propagates through `static { VMSupportsCS8 = ... }` and poisons `AtomicLong`, blocking every subsequent class that touches `AtomicLong`, `AtomicInteger` (shares the intrinsic), `CompletableFuture`, `ForkJoinTask`, `ThreadPoolExecutor`. Fixing rank-1 is likely to unblock both KC targets and reveal the next wave.

## Files

- `bench/missing-natives-kc.json` — sorted, diff-stable JSON census (this session).
- `docs/kc-missing-natives-census.md` — this document.
- Supporting evidence: `/tmp/kc16_stderr.txt` (historical StringUTF16.isBigEndian finding), `/tmp/kc26_d12_stderr.txt` (deepest KC26 classloader trace, 377 classes), `/tmp/kc26_final_stderr.log` / `/tmp/kc16_final_stderr.log` (Session 87 attempts).
