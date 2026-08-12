# `--jdk-only`: the completion roadmap

**Status: FINAL. Written 2026-08-12 from measurement, not from record titles.**

The goal, stated so it can be falsified: **any Java application that runs on
HotSpot 25 runs on `cratonvm --jdk-only`, with no synthetic class library
underneath it.** Not "the regression suite is green" — that suite is 72 small
deterministic vectors and it is already at 69 passed / 2 failed in both arms,
which licenses almost nothing about applications.

Three censuses were taken on 2026-08-12 and this plan is derived from them. Read
them before taking a lane; every number below is theirs, not this document's:

* APP-READINESS-20260812.md — what actually stops a real app, measured by
  running Tomcat, H2 and Spring on this host.
* STUB-CENSUS-20260812.md — the real shape of the native surface, per row, from
  a registry dump against a strict binary.
* RETIREMENT-20260812B.md — what is finished and why, per record.

---

## 0. What is already true, so nobody re-derives it

* **Embedded Tomcat 12 boots, serves GET/POST/404, and shuts down cleanly under
  `--jdk-only`.** Measured twice. This is the single most important fact in this
  document: the VM is much closer than the record count suggests.
* A 27-point sweep of the JDK surface real applications use passes **26/27** —
  reflection, annotations, proxies, `MethodHandles`, `URLClassLoader`,
  `ServiceLoader`, executors, locks, `CompletableFuture`, files, NIO, sockets,
  `HttpURLConnection`, `HttpClient`, JCA, XML.
* `--jdk-only` drops **every** `SyntheticStub` and nothing else (1282 → 0).
* Of 9342 natives that survive into strict mode, **only 692 are ones a JVM is
  obliged to provide**. The rest are class-library convenience.
* Category (B) — a stub whose refusal breaks working bytecode — measured
  **zero**. In one case strict is *more* correct than Compatible:
  `Collections.synchronizedList` loses elements in Compatible (6534 and 4697 of
  8000 across two runs) and is exact under `--jdk-only`.

**The corpus is not the constraint.** H2, Tomcat 12, Spring Framework 7.1,
Hibernate, Keycloak, WildFly, Elasticsearch, Kafka, commons-math and bc-java are
all built on this host. What is missing is runner setup, and both existing
runners can be bypassed by composing a classpath by hand from the built output.
The only genuinely absent corpus is Spring Boot.

---

## 1. PHASE 1 — the mechanism that blocks applications

**One defect shape, three reachable instances.** This is the whole of what stops
real applications today, and all three are independent lanes.

> A native registered in the **essential** set survives strict mode, runs, asks
> for a **fabricated** receiver, gets the refusal that `--jdk-only` exists to
> give, and dies as `NoClassDefFoundError` at the application's call site.
> **The refusal is correct. The survival of its caller is the defect.**

| lane | native | what it takes down |
|---|---|---|
| **P1-A** | `Atomic*FieldUpdater.newUpdater` → fabricated receiver | `java.sql.SQLException` holds a static `AtomicReferenceFieldUpdater`, so **the entire `java.sql` package is unloadable**. All JDBC, all ORM, all connection pools. It also corrupts exception identity far from JDBC: H2's `TestStringUtils` fails with "expected `DbException`, got `NoClassDefFoundError`". |
| **P1-B** | `System.getenv()` (no-arg only) → `cratonvm/internal/UnmodifiableMap` | **Spring dies in `AbstractEnvironment.<init>`**, before bean one. |
| **P1-C** | `SSLSocket.get{Output,Input}Stream()` → fabricated stream | Handshake succeeds, first stream access throws. **No HTTPS.** |

Two-line witness, reproduced twice per arm — `Class.forName("java.sql.SQLException")`
and `System.getenv()` both fail under `--jdk-only` and both pass on HotSpot.

**Attribution is clean and this is pre-existing, not campaign damage:** the
pristine `44044c7e2` control fails the JDBC probe identically, same stack.

**Prescriptions, each measurable on its own:**

* **P1-A** — either de-register the native under `JdkOnly` and let the real
  `AtomicReferenceFieldUpdaterImpl` run, or return a real object. The
  de-registration may resurrect the `ClassCastException` the native was written
  for: **measure that first**, do not assume either way.
* **P1-B** — the native already builds a real `HashMap`. Call the real
  `Collections.unmodifiableMap` on it instead of the fabricated wrapper. Note
  that `vm_init.rs::ensure_bootstrap_compat_class`'s premise — "these stand-ins
  exist for the synthetic collection shims, which strict mode does not
  register" — is **false for `UnmodifiableMap`**: `wrap_system_env_map` uses it
  and ships in the essential set.
* **P1-C** — do the strict-mode receiver work **separately from** async-close.
  The async-close fix is unsound as designed (see §3) and must not be bundled
  with this.

**Cross-cutting, cheap, do it first:** emit the refusal `warn!` at *every*
`try_ensure_synthetic_class`, not only the boot block. The startup banner
currently under-reports the blocker set by half — 13 of 27.

**Exit criterion for Phase 1:** `Class.forName("java.sql.SQLException")`,
`System.getenv()`, and an `SSLSocket` stream read all succeed under `--jdk-only`;
a JDBC probe reaches 10/10 and a Spring context constructs.

---

## 2. PHASE 2 — remove the stubs

The target is **not** the 1282 `SyntheticStub` natives; strict mode already drops
all of them and nothing breaks. The target is the population that *runs* in
strict mode and shadows real JDK bytecode:

| category | count | disposition |
|---|---|---|
| runs in strict, **bridge** | **3956** | the work |
| runs in strict, **intrinsic** | 499 | mostly legitimate acceleration (`Math`, `StringLatin1`) — **not roadmap work** |
| fabricated methods on real classes | 335 | delete; all tested families green without them |
| intercepts inherited/abstract declarations | 2865 | audit, lower priority |
| synthetic-jdk only | ~4390 call sites | invisible to both shipping binaries |

**What makes this parallel:** `registry.rs:5782` re-tags a Bridge to
`SyntheticStub` from a **central table**, so retiring a shadow needs no registrar
edit. One serialised lane (**P2-L0**) owns that table and receives nominations;
every other lane nominates and never edits it.

15 lanes, disjoint by registrar file, are enumerated in STUB-CENSUS-20260812.md
§5. P2-L1 (the 335 dead fabricated methods) and P2-L7 through P2-L13 can start
simultaneously today. P2-L5, P2-L6 and P2-L14 own the two largest files and must
run last and alone.

**Two immovables, named so nobody spends a lane on them:** `Object.<init>`
(1399 calls in one probe run) and the `Enum` family are object-layout concerns,
not class-library convenience.

**The honest limit:** proving each of the 3956 bridges *correct* is 3956
differential tests. The tractable form is to retire by family and let the corpus
adjudicate, which is why Phase 2 depends on Phase 4's corpus being wired first.

---

## 3. PHASE 3 — correctness gaps no stub census can see

These are invisible to every native census because they are not natives. Each is
an independent lane.

* **P3-A — the JIT omits the `aastore` covariance check.** `jit_aastore` has
  **no caller on any backend**; the JIT lowers `aastore` inline. Measured
  `cold=[java.lang.Integer] hot=[no-throw]` — a `String[]` slot accepting an
  `Integer` on the compiled tier only. Patch written in full in W7-37 Part 4,
  deliberately unapplied: it puts a Rust-boundary call on the hottest
  reference-store path in the VM and forces `has_dispatch` on nearly every
  compiled method. **Needs a store-heavy A/B, and the perf-preserving form is a
  per-site monomorphic inline cache on `(class_id_of(array), class_id_of(val))`.**
  This is the one live red in the suite (`RExceptions`).
* **P3-B — class-file parsing gap.** `MethodHandleProxies.asInterfaceInstance`
  fails with `ClassFormatError: ldc: unsupported constant pool entry type at #26`.
  Not a stub; a decoder gap, and a hard blocker for anything using it.
* **P3-C — typed linkage errors are flattened.** `native_classloader_define_class1`
  ends every failure with `define_class_format_error(...)`, so a preview class
  file yields `ClassFormatError` carrying a Rust `Debug` string
  (`Linkage(UnsupportedClassVersionError { class_name: "", … })`) where HotSpot
  throws `UnsupportedClassVersionError`. A container catching the JDK type does
  not catch ours.
* **P3-D — async close on TLS streams.** Four sites. **The obvious design is
  unsound**: socket readiness is not stream readiness, and a readiness gate
  deadlocks the ordinary HTTP-over-TLS shape because `rustls`' `wants_read()` is
  false while decrypted plaintext is buffered. The screen must be asked under
  the stream mutex (`conn.wants_read()` / `TlsStream::buffered_read_size()`)
  before releasing it. Pilot on `rustls_stream_read`'s client arm only;
  `*_write` must get no loop.
* **P3-E — `String.format` with no `Locale`** localises against ROOT instead of
  the FORMAT default (W7-91 §5).

---

## 4. PHASE 4 — the evidence base, which gates everything above

**The suite cannot see application defects, and Phase 2 cannot be adjudicated
without a corpus.** Three lanes, all independent, all startable now:

* **P4-A — wire the corpora that are already on disk.** H2, Tomcat, Spring
  Framework, Hibernate, Keycloak, WildFly, Elasticsearch, Kafka are built here;
  the runners want `mvn`/`ant`. Compose classpaths from built output instead.
  Spring Boot is the one genuine absence.
* **P4-B — run `--synthetic-jdk` MODE once.** It has never been executed, ever.
  Several records' residuals live only in that configuration and cannot be
  adjudicated any other way. Feature ≠ mode.
* **P4-C — compile the Linux arms.** `native-io/src/process.rs`'s non-Windows
  arms have never been compiled by any lane that edited them, and two ratchets
  plus the kind map are keyed `25/linux` so only a Linux run can re-freeze them.
  This also unblocks the `java/io/Print*` retirement and the
  `BootLoader.loadLibrary` arming, whose A/B **must** run on Linux — the Windows
  road is inert and a green Windows A/B measures the wrong road.

---

## 5. Instrument rules, paid for the hard way

* **The registry census works and the flag order matters.**
  `cratonvm --jdk-only --explain-jdk-only --dump-native-registry census.json -cp <cp> <Main>`.
  Placed **after** the main class the flag is silently ignored — no file, no
  warning, exit 0. `--explain-jdk-only` adds `image_declaring_method`, the
  adjudication against the class-path bytes; without it there is no four-way
  split.
* **`probes/` is never run by `run.sh` at any `SUITE=` value.** A record whose
  only evidence is a probe cannot be discharged by a suite run, however green.
* **`TIMEOUT=420` on this host** — `RMapGcStress` needs ~4m55s and times out at
  the 120s default, which then manufactures two `HARNESS ERROR` rows that read
  as independent defects.
* **A harness row under a failing vector is usually downstream of it.** A vector
  that dies emits no output, so the extract and count guards both fire.
* **Never freeze a number you cannot derive.** A firing gate is loud; a wrongly
  frozen one is silent forever. The stub ratchet's own text says so.
* **Non-null is not the contract.** Two defects survived this year behind
  `!= null` and count checks: three fabricated enum constants passed
  `values().length == 3`, and `Thread$State` passed non-null-and-named while
  `values()[0] == State.NEW` was false. Assert identity and equality.
* **Registration is the gate, and `NativeKind` is ambient.** Before changing a
  native, grep every registration of the triple, decide which wins on which boot
  path, and check the ambient kind of the winning block — and separately whether
  that registrar is reached at all in the mode you care about. A `Bridge`
  registrar behind `#[cfg(feature = "synthetic-jdk")]` is as dead in a shipping
  build as a `SyntheticStub` is in strict mode, and nothing refuses it loudly.

---

## 6. Order, and what can run at once

```
NOW, fully parallel:  P1-A   P1-B   P1-C   P4-A   P4-B   P4-C   P3-B   P3-C
                      + the try_ensure_synthetic_class warn! (one line)
AFTER P4-A lands:     P2-L1, P2-L7..L13  (parallel)      P3-A (needs the A/B)
                      P2-L0 serialised, receiving nominations from all
LAST, alone:          P2-L5  P2-L6  P2-L14   (the two largest registrar files)
DESIGN FIRST:         P3-D   (the obvious fix is unsound — do not bundle it)
```

**Definition of done.** Not a suite number. A Spring Boot application, a servlet
container serving HTTPS, and a JDBC workload each run to completion under
`--jdk-only` with no `cratonvm/internal/*` class instantiated — verifiable with
`--explain-jdk-only`, which names every fabricated receiver as it is refused.
