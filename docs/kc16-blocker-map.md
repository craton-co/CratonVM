# KC16 Boot-Blocker Map (Session 94, 2026-04-26)

## Live status (2026-04-26, Session 94 — fourth iteration; RVERIF.2 landed)

After RVERIF.2 (verifier subtype widening fix via JDK interface name table):

```
target/release/rustjvm.exe --java-home "C:/.../jdk-25.0.2.10-hotspot" \
   --Xmx 2g --jar /tmp/keycloak/keycloak-16.1.1/jboss-modules.jar -- \
   -mp /tmp/keycloak/keycloak-16.1.1/modules org.jboss.as.standalone \
   "-Djboss.home.dir=/tmp/keycloak/keycloak-16.1.1"
```

→ verifier passes. Single failure (no NoSuchMethodError, no other warnings):
```
B6: silent-swallow class=org/jboss/modules/Module
   exc=java/lang/NullPointerException: null object argument
Exception in thread "main" java/lang/NullPointerException
```

**RKC16N.9** — `org.jboss.modules.Module.<clinit>` NPE. The class init
is silently swallowed (B6 path), leaving `Module` in a partially-
initialised state, then `main()` references something that NPEs.
Investigation needed: turn off B6 or run with `--noverify` (now
identical without it) and capture the stack trace at NPE site.
Likely a static field that requires a real `Module` instance or a
`PathFilter`/`PathUtils` helper our minimal stubs don't return.



Keycloak 16.1.1 (WildFly / JBoss-Modules) under current `target/release/rustjvm.exe`
on Windows 11, JDK 25.0.2 (Adoptium) for boot classpath, default flags.

## Live status (2026-04-26, Session 94 — third iteration; RKC16N.1/3/5/8 + recon hacks landed)

After RKC16N.8 landed (`Class.desiredAssertionStatus()Z` + `System.initPhase1()V` stubs):

```
target/release/rustjvm.exe --java-home "C:/.../jdk-25.0.2.10-hotspot" \
   --Xmx 2g --jar /tmp/keycloak/keycloak-16.1.1/jboss-modules.jar -- \
   -mp /tmp/keycloak/keycloak-16.1.1/modules org.jboss.as.standalone \
   "-Djboss.home.dir=/tmp/keycloak/keycloak-16.1.1"
```

→ no NoSuchMethodError warnings. Single failure:
```
linkage error: verification error in org/jboss/modules/Module.getResources:
 at bytecode offset 294: expected ObjectRef("java/util/Collection") on stack,
 found ObjectRef("java/util/List")
```

That is **RVERIF.2** (subtype widening). Agent dispatched. With `--noverify`,
KC16 progresses to `org.jboss.modules.Module.<clinit>` NPE (RKC16N.9, follow-up).

`-version` (full clean run, no diagnostic warnings):
```
[rustjvm] stack-dump watchdog armed: will dump + abort after 45s
JBoss Modules version 2.0.0.Final
```

## Live status (2026-04-26, Session 94 — second iteration with recon hacks landed)

After Session 94 first-iteration work (Properties.load(Reader) stub +
String layout-neutral natives + override-allowlist + throwable ctor
stubs):

```
target/release/rustjvm.exe --java-home "C:/.../jdk-25.0.2.10-hotspot" \
   --Xmx 2g --jar /tmp/keycloak/keycloak-16.1.1/jboss-modules.jar -- \
   -mp /tmp/keycloak/keycloak-16.1.1/modules org.jboss.as.standalone \
   "-Djboss.home.dir=/tmp/keycloak/keycloak-16.1.1"
```

→ aborts at the bytecode verifier:
```
linkage error: verification error in org/jboss/modules/Module.getResources:
 at bytecode offset 294: expected ObjectRef("java/util/Collection") on stack,
 found ObjectRef("java/util/List")
```

This is the **RVERIF.1**-class bug (subtype widening): `List` extends
`Collection`, so the verifier should accept it.

With `--noverify`, KC16 progresses further to:
```
B6: silent-swallow — class=org/jboss/modules/Module exc=java/lang/NullPointerException
Exception in thread "main" java/lang/NullPointerException
```

`-version` (with the recon hacks) reaches the actual `main()` body and
prints `JBoss Modules version (unknown)` — the entirety of `Main.<clinit>`
runs cleanly.

**Frontier as of 2026-04-26 18:05 UTC**: WildFly module-loader inside
`org.jboss.modules.Module.<clinit>`. This is several phases past where
the previous KC16 baseline was stuck.

## Live status (2026-04-26)

Reproducer (worked from worktree root):
```
target/release/rustjvm.exe --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot" \
   --Xmx 2g --jar /tmp/keycloak/keycloak-16.1.1/jboss-modules.jar -- \
   -mp /tmp/keycloak/keycloak-16.1.1/modules org.jboss.as.standalone \
   "-Djboss.home.dir=/tmp/keycloak/keycloak-16.1.1"
```

Observed (~30 ms wall-clock to abort):
```
[rustjvm] stack-dump watchdog armed: will dump + abort after 45s
WARN  NoSuchMethodError method="java/lang/System.initPhase1()V"
WARN  NoSuchMethodError method="java/util/Properties.load(Ljava/io/Reader;)V"
Error in thread "main" linkage error: no such method:
   java/util/Properties.load(Ljava/io/Reader;)V
```

This is **earlier** than every blocker the previous (Session 93) revision of
this doc describes — the CHM `initTable` livelock at P0 module-loader
bootstrap is now unreachable because `org.jboss.modules.Main.<clinit>` aborts
during its `version.properties` load.

## Resolved since Session 93

| # | Status | Evidence |
|---|--------|----------|
| #3 KC16 `initPhase1` synthetic-stream fallback | RESOLVED | `WARN NoSuchMethodError java/lang/System.initPhase1()V` is now a non-fatal warning; main thread proceeds (it only re-fails downstream on Properties.load(Reader)). |
| #4 9 static MISSING natives | RESOLVED (WP0.3) | Coverage 191/191. |
| #5 Silent livelock / no watchdog | RESOLVED | First line of every run is `stack-dump watchdog armed: will dump + abort after 45s`. SIGTERM dump still TBD. |

## Open blockers (refreshed)

| # | Phase | Symptom | Root cause | Fix scope | Cmplx |
|---|-------|---------|------------|-----------|-------|
| 1 | Mod-loader pre-P0 | **`Properties.load(Ljava/io/Reader;)V` NoSuchMethodError** in `Main.<clinit>` reading `version.properties`. | `properties_sidetable.rs::register_properties_sidetable` registers `load(InputStream)` but not `load(Reader)`. | New native that drains the Reader via `invoke_virtual(read([CII)I)` and reuses `parse_properties`. See **RKC16N.1** in `roadmap-any-java-app.md`. | Easy |
| 2 | Mod-loader P0 (gated on #1) | CHM `initTable()` CAS livelock on `sizeCtl` (PC 0..41 spin ~5,740 /s). Identical bytecode to KC26 #1. | `Unsafe.compareAndSetInt` reads zero-alloc primitive slot as `Value::Object(None)` not `Int(0)`; CAS equality never holds. | Typed default at `gc::heap::alloc_object`, or typed `read_slot`. See **RKC16N.2**. | Hard |
| 3 | Mod-loader P0 (gated on #2) | Never opens any `module.xml`. | Downstream of #2. | — | — |
| 4 | Boot diff | Synthesize `[L…;` array classes on demand instead of JMOD scan. | KC26 Blocker #3 (still open across both KC16 and KC26). | `classloading/src/class_manager.rs`. See **RKC16N.3**. | Easy |
| 5 | Observability | SIGTERM/abort path still loses missing-natives audit dump. | Audit flush only on clean exit. | Flush on watchdog/SIGTERM. | Medium |

Workload (legacy reference): `jboss-modules.jar -jaxpmodule
javax.xml.jaxp-provider org.jboss.as.standalone -b 0.0.0.0`. JDK 21.0.6.
Reference: HotSpot `standalone\log\server.log` hit `WFLYSRV0025` in ~47 s.

---

## Historical notes (Session 93 / WP0.3, 2026-04-24)

**WP0.3 now closed** (N1..N4 + WP0.3 verification):
  * All 9 known MISSING natives registered. `s10_native_coverage_100_percent`
    runs: `Total ACC_NATIVE=191, Covered=191, Missing=0, Coverage=100.0%`.
  * Anchored grep `MISSING: .+-> Native\.` (roadmap pattern, legacy format)
    returns 0 — the real emitter at `vm/src/vm/vm_object.rs:719` emits
    `MISSING: <class>.<method><desc>` and the inventory scan returns empty.
  * Blocker #4 below is therefore RESOLVED.

## 1. WildFly phase timeline

`RUST_LOG=rustjvm_vm=trace` → `/tmp/kc16_d2_stderr.txt` (4.0 GB, 20.1 M lines, killed at 206 s).

| Phase | Event | Time |
|-------|-------|------|
| VM startup, 70 jmods, 267 classes | `NativeBridge Coverage 192/201`, 9 MISSING | 03:10:17.267 |
| JDK core clinits | `<clinit> java/util/Collections` | 03:10:17.380 |
| **`org.jboss.modules.Main.<clinit>`** | + `StartTimeHolder`, `Properties` | 03:10:17.383 |
| `ConcurrentHashMap.<clinit>` + `version.properties` | bytes=732 | 03:10:17.386 |
| StringUTF16 compress, Preconditions clinit | — | 03:10:17.444 |
| **LIVELOCK — `Ifeq(-41)` in `CHM.initTable()` CAS** | — | 03:10:17.449 |
| 206 s, 1,184,080 iterations (~5,740 /s) | — | kill |

**Phase reached: P0 Module-loader bootstrap.** Never parses `module.xml`,
never resolves `org.jboss.as.standalone`, never reaches MSC, subsystems,
Keycloak deployment, or HTTP listener.

## 2. Top 5 blockers

| # | Phase | Symptom | Root cause | Fix | Cmplx |
|---|-------|---------|------------|-----|-------|
| 1 | Mod-loader P0 | **CHM `initTable()` CAS livelock** on `sizeCtl`: PC 0..41 spin 5,740 /s. Identical bytecode to KC26 #1. | `Unsafe.compareAndSetInt` reads zero-alloc primitive slot as `Value::Object(None)` not `Int(0)`; CAS equality never holds. | Typed default at `alloc_object` or typed `read_slot`. | **Hard** |
| 2 | Mod-loader P0 | Never opens any `module.xml`. | Downstream of #1. | Gated on #1. | Hard |
| 3 | Bootstrap diff | **No `initPhase1` synthetic-stream fallback** (KC26 emits it). | Different `-jar` launch paths. | Align with KC26 after #1. | Medium |
| 4 | Natives | ~~9 static MISSING~~ **RESOLVED (WP0.3, Session 93)** — all 9 (`Class.getProtectionDomain0/getSigners/setSigners`, `Thread.sleep0`, 4× `AccessController.*`, `AtomicLong.VMSupportsCS8`) registered with real implementations (not stubs). Coverage now 191/191 = 100 %. | — | Done. | Done |
| 5 | Observability | Silent livelock: no WARN, no stuck-thread detector, `kill -9` loses audit. | No heartbeat, no SIGTERM flush. | `--XX:StuckThreadMs=N` + audit flush. | Medium |

## 3. New missing natives beyond the 9 known

**None.** 0 dynamic `UnsatisfiedLinkError`, 0 swallows in 4.0 GB trace —
only 10 startup-banner lines. Livelock precedes any further native dispatch.
Post-WP0.3: the original 9 banner lines are gone — the static scan now
reports 0 MISSING. Next time KC16 is re-run for a fresh census, any new
misses will be **dynamic** (reached only after the CHM livelock clears via
T19-wave-1).

## 4. Final state at 120 s (ran 206 s)

Process alive, 1 thread, ~5,740 ops/s. Interpreter looping PC 0..41 of
`CHM.initTable`. 26 classes fully clinit'd (vs KC26's 389 — KC16 trips
earlier, during `jboss-modules/Main`'s first CHM use, before JDK finishes
`j.u.concurrent`). 0 swallows, 0 exceptions, `server.log` not created,
stdout empty.

## 5. Shared-with-KC26 vs KC16-specific

| Blocker | KC26 | KC16 |
|---------|------|------|
| **CHM `initTable` CAS livelock (#1)** | YES | **YES — identical fix** |
| 9 static-scan MISSING natives | ~~YES~~ **RESOLVED** | ~~YES~~ **RESOLVED** (WP0.3) |
| `initPhase1` synthetic-stream fallback | YES | **NO — KC16-differential** |
| `[L…;` array synthesis (KC26 #3) | YES | unreachable |
| Silent livelock / no watchdog (#5) | YES | YES |
| WildFly MSC/Undertow/XNIO (T19.1/2/7) | N/A | deferred, gated on #1 |
| Quarkus ArC/static-init (T19.3/4) | YES | N/A |

**Both share the same primary blocker in the same JDK class.** Fixing #1
unblocks both; all version-specific work is behind that choke point.

## 6. Recommended next-session work

1. **Fix #1** per `docs/kc26-blocker-map.md` §5.2. Unblocks KC16 + KC26 together.
2. **#5** — `--XX:StuckThreadMs=N` + SIGTERM flush. Otherwise every future hang needs a 4 GB trace.
3. Re-run KC16 after #1 → expect P1 (`module.xml`) and fresh natives (JBoss-VFS, Log-Manager, Elytron).
4. **Defer T19.1 / T19.2 / T19.7** — unreachable today.
5. Re-run census; expect KC16 natives (JGroups, JBoss-Threads, Elytron) beyond the shared 9.

## Artefacts

* `/tmp/kc16_d2_stderr.txt` — 4.0 GB trace
* `/tmp/kc16_d2_stdout.txt` — 0 bytes
* `C:\craton\keycloak-16.1.1\standalone\log\server.log` — HotSpot reference
* `docs\kc26-blocker-map.md` — D1 peer
* `docs\kc-missing-natives-census.md` — static baseline

#1 gates T19.1-4; #5 is a new T19 observability sub-task. WP0.3 closed:
9 MISSING natives all registered — no residual T10 work on this list.
