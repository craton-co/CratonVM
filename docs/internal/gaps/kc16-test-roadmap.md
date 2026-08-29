# Roadmap — CratonVM runs any Java app (general JDK API gap closure)

**Anchor (Session 102, commit `da5d0a4`):** all JBoss/KC-specific stubs
stripped. KC16 boot regresses to its pre-Session-99 state but the
codebase no longer contains application-specific glue that doesn't
generalize. Smoke matrix (9 representative apps) green.

This doc replaces the prior KC16-only path. The new principle:

> **No application-specific stubs.** Every fix must close a JDK API or
> JVM-spec gap that benefits multiple unrelated apps. KC16 remains a
> useful diagnostic but only fixes that generalize beyond it land.

For the canonical "runs any Java app" surface inventory, see
[docs/roadmap-any-java-app.md](./roadmap-any-java-app.md). This doc
defines the **wave dispatch protocol** layered on top.

## Forcing functions (rotate; don't fixate on one)

| App | Why it forces general work |
|---|---|
| Apache Maven 3.9 (`mvn --version`) | Heavy XML, classloading via `plexus-classworlds`, reflection, ServiceLoader |
| Apache Tomcat 10 embedded | HTTP, NIO `Selector`, `ServerSocketChannel`, threading, classloading |
| Spring Boot 3 fat-jar | Nested-JAR classloading, autowiring (reflection), embedded server |
| Apache Cassandra (boot only) | NIO, JFR, threading, off-heap memory |
| Apache Kafka 3 (single broker) | NIO, NIO selector, ZooKeeper-less mode |
| The 32 apps in `apps/` | Broad spec coverage; cheap to re-run |
| Keycloak 16 | Useful diagnostic for JBoss-Modules/CDI ecosystem; **fixes only land if generalizable** |

Each wave should pick **at least 2 forcing functions** and verify the
fix moves both forward (or one forward + one unchanged — never one
forward + one regressed).

## Operational rules baked into every prompt

(Carried forward from Sessions 96-101 lessons.)

- **Mandatory `CRATONVM_STRICT_SWALLOWS=1` trace** before any code change
  on `<clinit>` / native-resolution failures. Last 30 lines verbatim
  in report.
- **Mandatory build + repro + smoke verification** before reporting.
  Verbatim output of each step in report.
- **Worktree baseline check** as step 0: if HEAD is not the latest
  main, merge `claude/intelligent-ishizaka-6d18f0` first.
- **Tight timebox**: 3 build-test cycles maximum.
- **No application-specific stubs**: any new file matching
  `org/<vendor>/*` or `<vendor>_*.rs` must be justified with two
  unrelated forcing functions that benefit. JDK-spec implementations
  (`java.*`, `javax.*`, `jdk.internal.*`) are always in scope.
- **Single-file scope** preferred; multi-file when registration in
  `lib.rs` is needed.
- **Restricted files**: `vm/src/runtime/value_stack.rs`, Getfield/Putfield
  blocks of `vm/src/runtime/interpreter.rs`,
  `native-builtins/src/phases_late.rs::register_phase71_natives`.
- **CI gate**: `scripts/check-no-diag-prints.sh` must pass.
- **Batch size**: ≤4 opus agents in flight at once.

---

# WAVE 1 — General JDK gap closure (4 agents, 1 day)

**Goal**: close 4 general JDK API gaps that show up across multiple
forcing functions. Each task picks 2 unrelated apps to verify the fix
generalizes.

## W1-A — JMX MXBean expansion: MemoryPool, MemoryManager, GarbageCollector

| | |
|---|---|
| Repro | Java probe `apps/jmx_probe/JmxProbe.java` (write it):  `ManagementFactory.getMemoryPoolMXBeans()`, `getMemoryManagerMXBeans()`, `getGarbageCollectorMXBeans()`; print `.size()` and `.get(0).getName()` for each. |
| Cross-app verify | (1) JmxProbe — direct test. (2) Run any app with `-Dcom.sun.management.jmxremote=true` (HelloWorld is fine) and verify no new ULE swallows. |
| Success | JmxProbe matches HotSpot output. KC16 boot's ManagementFactory ULE swallow gone (incidental — Sessions 97/99 already addressed VMManagementImpl; this is the next layer). |
| Files | `native-builtins/src/jmx.rs` (extend `register_vm_management_impl` pattern). |
| Generalizes to | Any app monitored by JMX (Prometheus exporters, Spring Actuator, JConsole, profilers, IDE debuggers). |

## W1-B — `ClassLoader.getResources()` correctness audit

| | |
|---|---|
| Repro | Java probe `apps/classloader_probe/ClProbe.java`: lookup `../../../apps/META-INF/services/foo.svc` from a synthesized JAR + `../../../apps/META-INF/MANIFEST.MF` from `rt.jar`/jimage; verify both return at least one URL each. The S96 RSLF4J.1 fix landed for the first; verify the second path (boot-loader resources) too. |
| Cross-app verify | (1) ClProbe — direct test. (2) Apache Maven `mvn --version` — currently fails because Maven uses `plexus-classworlds` which calls `getResources` heavily for plugin discovery. Spawn a tiny `apache-maven-3.9.6` install in `/tmp/maven` and try `target/release/cratonvm.exe --java-home <jdk> -c "/tmp/maven/boot/plexus-classworlds-2.8.0.jar" org.codehaus.plexus.classworlds.launcher.Launcher --version`. Confirm the failure (or success) gets visibly closer. |
| Success | ClProbe + plexus-classworlds Launcher both find their resources. |
| Files | `native-builtins/src/classloader.rs`, possibly `classloading/src/loaders.rs`. |
| Generalizes to | Maven, every JDBC driver, SLF4J/Logback, Jackson modules, charset providers — every classpath-JAR ServiceLoader user. |

## W1-C — `ThreadPoolExecutor` / `ExecutorService` audit

| | |
|---|---|
| Repro | Java probe `apps/executor_probe/ExecProbe.java`: `ExecutorService es = Executors.newFixedThreadPool(4); es.submit(() -> 42).get();` returns 42. Then a 4-thread × 1000-task throughput test; assert all complete. |
| Cross-app verify | (1) ExecProbe. (2) `apps/cf_probe/CfMin` (already passes today). (3) Spring Boot 3 minimum: `java -jar petclinic.jar` reaches "Started PetClinicApplication in N seconds" line. |
| Success | All three pass. |
| Files | `native-builtins/src/concurrent.rs`, `native-builtins/src/lang_thread.rs`. |
| Generalizes to | Tomcat, Netty, Spring async, anything using `Executors`, every web server. |

## W1-D — JAXP / StAX XML parsing for arbitrary files

| | |
|---|---|
| Repro | Java probe `apps/xml_probe/XmlProbe.java`: `XMLInputFactory.newInstance().createXMLStreamReader(new FileInputStream("/tmp/test.xml"))`; advance via `next()`; assert `getLocalName()` matches expected on first START_ELEMENT. Use a test fixture XML that includes nested elements, attributes, and CDATA. |
| Cross-app verify | (1) XmlProbe. (2) Maven again — `pom.xml` parsing is StAX-based. (3) Hibernate `hibernate.cfg.xml` parsing (small fixture). |
| Success | XmlProbe + at least one of {Maven, Hibernate} fixture parses. |
| Files | `native-builtins/src/xml_stax.rs` (new) + `lib.rs` registration. |
| Generalizes to | Maven, Spring (XML config), Hibernate (`hibernate.cfg.xml`), every build tool, every EE app. |

---

# WAVE 2 — Reflection + ServiceLoader hardening (3 agents)

**Goal**: full reflective surface coverage so DI containers + annotation scanners can run.

## W2-A — `Class.getDeclaredFields` / `getDeclaredMethods` / `getDeclaredConstructors` correctness
(Per RC.1-RC.3 in roadmap-any-java-app.md.) Verify against (1) AnnoTest, (2) Spring's `ClassUtils.getDeclaredMethods`, (3) Jackson's `BeanDescription`.

## W2-B — `Method.invoke` + `Constructor.newInstance` boxing/varargs
(Per RC.5-RC.6.) Verify against (1) AnnotationProxyProbe, (2) Spring `MethodIntrospector`.

## W2-C — `MethodHandles.Lookup.findVirtual/findStatic/findSpecial`
(Per RC.8.) Verify against (1) FindSpecialProbe, (2) lambda-heavy app like `apps/cf_probe/CfAsync`.

---

# WAVE 3 — NIO + Networking (4 agents)

**Goal**: HTTP servers / clients work end-to-end.

## W3-A — `Socket.connect` + `ServerSocket.accept` real TCP
(Per RE.1-RE.2.) Verify against (1) loopback echo test, (2) Tomcat embedded responding to `curl /`.

## W3-B — `URL.openConnection().getInputStream()` for HTTP
(Per RE.4.) Verify against (1) loopback test, (2) `Maven plugin download` from local file:// URL.

## W3-C — `Selector.select` NIO
(Per RE.9.) Verify against (1) Netty echo, (2) Tomcat NIO connector.

## W3-D — `HttpClient.send` (JDK 11+ HttpClient)
(Per RE.5.) Verify against (1) curl-like probe, (2) Spring `RestTemplate` minimal.

---

# WAVE 4 — Concurrency primitives (3 agents)

(Per RD.1-RD.10.) Each verifies against ≥2 apps from `apps/` plus one external forcing function.

---

# WAVE 5 — Crypto (3 agents)

(Per RF.1-RF.10.) Verify against (1) DigestProbe/CipherProbe (already pass), (2) `KeyStore` load from real cacerts, (3) `HttpsURLConnection` against a public HTTPS endpoint.

---

# WAVE 6 — Real-app smoke matrix (5+ agents, ongoing)

For each forcing function in the table at top: write a smoke fixture under `bench/<app>/`, get it green. Each fixture is one agent task.

| App | Smoke target |
|---|---|
| Apache Maven 3.9 | `mvn --version` exits 0 |
| Apache Tomcat 10 embedded | `curl http://localhost:8080/` returns 200 |
| Spring Boot 3 (Petclinic) | "Started PetClinicApplication" log line |
| Apache Cassandra (boot only) | "Listening for thrift clients" log line |
| Apache Kafka 3 (single broker) | "Kafka Server started" log line |
| H2 1.4 (embedded SQL) | `CREATE TABLE … INSERT … SELECT` round-trip |

---

# Wave dependencies

```
W1 (general gaps) -> W2 (reflection) -> W3 (NIO/net) -> W4 (concurrency) -> W5 (crypto) -> W6 (real apps)
```

Within a wave, tasks are pairwise file-disjoint and parallel-safe. The
list under each wave is the dispatch slate.

W6 is **continuous** — re-run after every wave to catch regressions
and surface new gaps. Each forcing function picks up new failures as
the underlying surface fills in.

# How to dispatch a wave

1. Pick the next wave whose dependencies are all green.
2. For each task in the wave, draft a self-contained agent prompt
   following the **Session 101 6-step template** (the proven one):
   1. Worktree-baseline merge (step 0)
   2. `cargo build --release -p cratonvm-cli`
   3. Run baseline repro, capture last 10 lines verbatim
   4. Run `CRATONVM_STRICT_SWALLOWS=1` trace, capture panic backtrace
   5. Implement fix; rebuild
   6. Run post-fix repro, capture last 10 lines verbatim
   7. Smoke regression on 2-3 apps
   - Report format explicitly enumerated; "if your final report does
     not include all artifacts, you have not completed the task"
3. Dispatch ≤4 agents in parallel via the harness.
4. Wait for completions; merge usable patches; commit one session per
   wave with the same prose-sectioning style as Sessions 96-101.
5. Re-run the smoke matrix from Wave 6 to confirm no regression.
6. Update this doc with the wave's outcome before dispatching next wave.

# Anti-pattern checklist (ANY Yes blocks the merge)

When reviewing an agent's diff before merging:
- [ ] Does it add a file matching `*<vendor>*.rs` for vendor in
  `{jboss, wildfly, keycloak, undertow, weld, hibernate, resteasy,
  springframework, tomcat, jetty, ...}`? **YES → reject.**
- [ ] Does it stub a method that only one application calls? **YES → reject.**
- [ ] Is the fix justified by exactly one app and not generalizable to a
  second? **YES → reject.**
- [ ] Does the fix paper over a JDK-spec gap (e.g. swallow the error
  instead of fixing the producer)? **YES → reject.**

JDK-spec fixes are always in scope: anything in `java.*`, `javax.*`,
`jdk.internal.*`, `sun.*` if reachable from JDK 25 boot.

# Estimated effort

| Wave | Agents | Per-agent effort | Wave wall-time @ 4 parallel |
|---|---|---|---|
| W1 | 4 | 0.5–1d | 1d |
| W2 | 3 | 1–2d | 2d |
| W3 | 4 | 2–4d | 1 week |
| W4 | 3 | 1–2d | 2-3d |
| W5 | 3 | 1–2d | 2-3d |
| W6 | 5+ | 2–5d | continuous |

**Realistic total**: 6-8 weeks of focused work for waves 1-5 + smoke
matrix. W6 keeps going forever (more apps = more coverage).
