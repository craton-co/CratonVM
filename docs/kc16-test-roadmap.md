# Roadmap — Keycloak 16 starts and passes its tests

**Anchor (Session 101, commit `88d3f71`):** KC16 main() runs to a clean
`System.exit(1)` with zero swallowed errors. With `RUSTJVM_SOFT_EXIT=1`,
main() reaches normal completion past `org.jboss.as.server.Main.abort`.
WildFly's `ServiceContainer` still does not actually start: `server.log`
is never written by WildFly itself, no port is bound, no admin console.

This roadmap defines the path from "main() exits cleanly" to "Keycloak
serves OIDC tokens against a test realm". Each wave is broken into
**atomic agent tasks** that are pairwise file-disjoint, single-file
scope where possible, with concrete reproducers and binary success
criteria. Each task is sized for ≤1 day for an isolated background
agent.

## Format conventions

Each task has:

| Field | Meaning |
|---|---|
| ID | `W<wave>-<letter>` — stable handle |
| Title | Imperative, <60 chars |
| Repro | Exact shell command proving "before" |
| Success | Concrete after-state, observable in one command |
| Files | Primary files the agent will touch |
| Blocks | Other tasks gated on this |
| Parallel-safe with | Pairwise compatibility |

## Operational rules baked into every prompt

- **Mandatory `RUSTJVM_STRICT_SWALLOWS=1` trace** before any code change
  on `<clinit>` / native-resolution failures. The trace's last 30 lines
  must appear in the agent's report (Session 101 lesson — the
  Session 100 first-attempt agent skipped this and produced no work).
- **Mandatory build + repro + smoke verification** before reporting
  success. Report must include verbatim output of each step.
- **Worktree baseline check** as step 0: if HEAD is not the latest
  main, merge `claude/intelligent-ishizaka-6d18f0` first.
- **Tight timebox**: 3 build-test cycles maximum. Ship the closest
  working state if exhausted; document the gap in the report. Do not
  iterate into a watchdog timeout.
- **Single-file scope** preferred. Two-file changes only when
  registration in `lib.rs` is needed.
- **Restricted files (do not modify)**: `vm/src/runtime/value_stack.rs`,
  Getfield/Putfield blocks of `vm/src/runtime/interpreter.rs`,
  `native-builtins/src/phases_late.rs::register_phase71_natives`.
- **CI gate**: `scripts/check-no-diag-prints.sh` must pass.
- **Batch size**: ≤4 opus agents in flight at once (Session 100 lesson —
  6 simultaneous agents exhausted account capacity).

---

# WAVE 1 — Visibility past Main.abort  (3-4 agents)

**Goal**: with `RUSTJVM_SOFT_EXIT=1` set, KC16 boot reaches at least
one named WildFly subsystem init phase. Currently main() returns
cleanly after the soft-exit but produces no further visible output.

## W1-A — Post-soft-exit failure cataloger  *(investigation, no code)*

| | |
|---|---|
| Repro | `RUSTJVM_SOFT_EXIT=1 RUST_LOG=warn target/release/rustjvm.exe --java-home <jdk25> --Xmx 2g --jar /tmp/keycloak/keycloak-16.1.1/jboss-modules.jar -- -mp /tmp/keycloak/keycloak-16.1.1/modules org.jboss.as.standalone "-Djboss.home.dir=/tmp/keycloak/keycloak-16.1.1" 2>&1 \| tee /tmp/kc16_w1a.log` |
| Success | `/tmp/kc16_w1a.log` contains at least one line beyond `[rustjvm] System.exit(1) soft-returned`. Report categorizes each unique post-exit failure into a numbered W2-* entry filed as a follow-up section in this doc. |
| Files | `docs/kc16-test-roadmap.md` (this file — Wave 2 section append only). |
| Parallel-safe with | All other W1-* tasks. |

## W1-B — Defensive JMX expansion: MemoryPoolImpl + MemoryManagerImpl + GarbageCollectorImpl

| | |
|---|---|
| Repro | Pre-emptive — no current swallow attributable to this. Test fixture: a Java probe that calls `ManagementFactory.getMemoryPoolMXBeans()`, `getMemoryManagerMXBeans()`, `getGarbageCollectorMXBeans()` and prints `.size() + " " + .get(0).getName()` for each. Verify against HotSpot first. |
| Success | Probe runs to OK against rustjvm; sizes match HotSpot. KC16 boot rc=0 unchanged. |
| Files | `native-builtins/src/jmx.rs` (extend per RKC16N.10/.12 pattern). New test under `apps/jmx_probe/`. |
| Parallel-safe with | All. |

## W1-C — JNDI scaffolding: `javax.naming.InitialContext.lookup` returns non-null for the canonical JBoss JNDI prefixes

| | |
|---|---|
| Repro | Java probe: `new InitialContext().lookup("java:jboss/")`. Currently throws or returns null. WildFly heavily uses JNDI for service binding — without this, no MSC service registers. |
| Success | The probe returns a non-null `Context` object whose `list("")` returns an empty NamingEnumeration (not null). |
| Files | `native-builtins/src/jndi.rs` (new) + `lib.rs` registration. |
| Parallel-safe with | W1-B, W1-D. |

## W1-D — Module classloader parent-first visibility test

| | |
|---|---|
| Repro | Java probe loaded via `-mp <modules> <main-class>` that calls `Thread.currentThread().getContextClassLoader().loadClass("java.lang.String")` AND `loadClass("org.jboss.as.server.ServerEnvironment")`. Both must succeed. |
| Success | Both `loadClass` calls return non-null Class objects. JDK classes resolve via parent (boot) loader; module classes resolve via module loader. |
| Files | `native-builtins/src/jboss_module_loader.rs` (verify + test). |
| Parallel-safe with | All. |

---

# WAVE 2 — MSC ServiceContainer bootstrap  (4-5 agents)

**Goal**: WildFly's `ServiceContainer` reaches `start()` and registers at
least one service. `org.jboss.msc.service.ServiceContainerImpl.<init>` runs
without swallow.

This wave's exact agent list comes from W1-A's catalog. Templates below
cover the most likely failures based on prior WildFly experience.

## W2-A — Fix topmost MSC init blocker  *(filled by W1-A)*

| Field | TBD by W1-A |

## W2-B — `org.jboss.msc.service.ServiceName` interning

| | |
|---|---|
| Repro | Java probe: `ServiceName.JBOSS.append("server")` returns a non-null ServiceName whose `getCanonicalName()` is `"jboss.server"`. |
| Success | Probe passes; same hashCode for two equivalent ServiceName instances. |
| Files | Likely `native-builtins/src/jboss_msc.rs` (new). |

## W2-C — `java.util.concurrent.ThreadFactory` + `ThreadPoolExecutor` for MSC

| | |
|---|---|
| Repro | Java probe: `Executors.newFixedThreadPool(2).submit(() -> 42).get()` returns 42. Today (per docs/jdk-regression-baseline.md) the basic case works; verify under MSC's specific ThreadFactory pattern (named threads, daemon flag, exception handler). |
| Success | Probe passes with exact thread name `MSC service thread 1-1`. |
| Files | `native-builtins/src/concurrent.rs` (extend). |

## W2-D — XML parsing: `standalone.xml` opens via JAXP/StAX

| | |
|---|---|
| Repro | Java probe: `XMLInputFactory.newInstance().createXMLStreamReader(new FileInputStream("/tmp/keycloak/.../standalone/configuration/standalone.xml"))` returns a reader whose `next()` advances to START_ELEMENT with localName "server". |
| Success | Probe runs to OK, reads the first ~10 elements without exception. |
| Files | `native-builtins/src/xml_stax.rs` (new or extend). |
| Notes | StAX is independently useful for many WildFly subsystems. |

## W2-E — `Thread.UncaughtExceptionHandler` plumbing

| | |
|---|---|
| Repro | Java probe spawns a thread, sets an UncaughtExceptionHandler, throws inside; handler must fire. WildFly's MSC threads rely on this. |
| Success | Handler captures the exception. |
| Files | `native-builtins/src/lang_thread.rs`. |

---

# WAVE 3 — Subsystem init  (5-6 agents)

**Goal**: WildFly logs `WFLYSRV0039: Creating http management service` (or
similar — first per-subsystem log line). HTTP listener binds.

## W3-A — Logging subsystem: `org.jboss.logmanager.LogManager` end-to-end

| | |
|---|---|
| Repro | KC16 boot with `RUSTJVM_SOFT_EXIT=1` writes a `WFLYLOG0001` startup line to `<base>/standalone/log/server.log`. |
| Success | `head -1 standalone/log/server.log` matches `WFLYLOG0001 .* Logging subsystem started`. |
| Files | `native-builtins/src/jboss_logmanager.rs` (extend the Block 2C shim with file-handler + pattern formatter). |

## W3-B — IO subsystem: XNIO worker pool

| | |
|---|---|
| Repro | Java probe: `org.xnio.Xnio.getInstance().createWorker(OptionMap.EMPTY)` returns a non-null XnioWorker whose `getName()` matches `"XNIO-1"`. |
| Success | Probe passes. |
| Files | `native-builtins/src/xnio.rs` (new). XNIO is JBoss's NIO abstraction; ~20 classes need stubs. |

## W3-C — Undertow subsystem: `HttpServer` binds port 8080

| | |
|---|---|
| Repro | KC16 boot with `RUSTJVM_SOFT_EXIT=1`; in another shell `curl -sI http://localhost:8080/` returns ANY HTTP response (4xx/5xx is fine — just need bytes back). |
| Success | curl exit 0 + at least one `HTTP/1.1` line in the response. |
| Files | Likely `native-builtins/src/undertow.rs` (new) + verify our `java.net.ServerSocket` / NIO selectors work end-to-end. |
| Blocks | All of Wave 4-6. |

## W3-D — Naming subsystem (extends W1-C)

| | |
|---|---|
| Repro | After KC16 boot, JBoss LogManager logs `WFLYNAM0001 .* Naming subsystem started`. |
| Success | The line appears in server.log. |
| Files | `native-builtins/src/jndi.rs`. |

## W3-E — Datasources subsystem: H2 driver registers via ServiceLoader

| | |
|---|---|
| Repro | After KC16 boot, log contains `WFLYJCA0004 .* Deploying JDBC-compliant driver class org.h2.Driver`. |
| Success | The line appears in server.log. |
| Files | Verify RSLF4J.1's classpath-JAR-ServiceLoader fix works for `META-INF/services/java.sql.Driver`; extend `native-builtins/src/jdbc.rs` if needed. |

## W3-F — Elytron / TLS: `KeyStore.getInstance("PKCS12")` works against a fixture P12

| | |
|---|---|
| Repro | Java probe loads `<base>/standalone/configuration/application.keystore` via `KeyStore.getInstance("PKCS12").load(fis, password)`; `aliases()` returns at least 1 alias. |
| Success | Probe passes. |
| Files | Verify against `docs/roadmap-any-java-app.md::RF.8`. Implement if not present. |

---

# WAVE 4 — Keycloak deployment  (5-6 agents)

**Goal**: Keycloak's WAR deploys. Logs `WFLYUT0021 .* Registered web context: '/auth' for server 'default-server'`.

## W4-A — Deployment-scanner picks up `keycloak-server.war`

| | |
|---|---|
| Repro | After KC16 boot, log contains `WFLYDS0019 .* Deployment scanner` AND `Started deployment of "keycloak-server.war"`. |
| Files | `native-builtins/src/wildfly_deployment.rs` (new). |

## W4-B — CDI / Weld init: `BeanManager` is created

| | |
|---|---|
| Repro | Java probe (deployable as a tiny WAR) injects `@Inject BeanManager bm` and prints `bm.getBeans(Object.class).size()`. |
| Success | Probe prints non-zero. |
| Files | `native-builtins/src/cdi.rs` (new). Weld is a substantial dependency; this stub may need only enough to satisfy KC's bean-discovery scan. |

## W4-C — JPA / Hibernate: `EntityManagerFactory` for KC's `keycloak-default` PU

| | |
|---|---|
| Repro | After deploy, log contains `WFLYJPA0010 .* Starting Persistence Unit Service 'keycloak-server.war#keycloak-default'`. |
| Files | `native-builtins/src/jpa.rs` (new). |

## W4-D — Resteasy / JAX-RS: REST endpoint resolution

| | |
|---|---|
| Repro | After deploy, log contains `RESTEASY002225 .* Deploying javax.ws.rs.core.Application: org.keycloak.services.resources.KeycloakApplication`. |
| Files | `native-builtins/src/resteasy.rs` (new). |

## W4-E — Keycloak SPI registry: `Spi` services discovered

| | |
|---|---|
| Repro | After deploy, log contains `KC-SERVICES0001 .* Loading config from standalone.xml`. |
| Files | Verify ServiceLoader path for `META-INF/services/org.keycloak.provider.Spi`. |

## W4-F — Theme + static resources: admin/keycloak themes resolve

| | |
|---|---|
| Repro | After deploy, log contains `WFLYUT0021 .* Registered web context: '/auth/resources'`. |
| Files | Resource-loading paths in `native-builtins/src/classloader.rs`. |

---

# WAVE 5 — Live HTTP smoke  (3-4 agents)

**Goal**: Keycloak responds to HTTP requests. Each task is a binary curl check.

## W5-A — Welcome page: `GET /` returns 200

| | |
|---|---|
| Repro | KC16 booted in a background process; `curl -sI http://localhost:8080/auth/` returns `HTTP/1.1 200`. |
| Success | curl exit 0 + status line `200`. |

## W5-B — OIDC discovery endpoint

| | |
|---|---|
| Repro | `curl -s http://localhost:8080/auth/realms/master/.well-known/openid-configuration \| jq -r .issuer` returns `http://localhost:8080/auth/realms/master`. |
| Success | The expected issuer string. |

## W5-C — Master realm exists and is reachable

| | |
|---|---|
| Repro | `curl -s http://localhost:8080/auth/realms/master \| jq -r .realm` returns `master`. |
| Success | Realm name in JSON. |

## W5-D — Admin console returns HTML

| | |
|---|---|
| Repro | `curl -sI http://localhost:8080/auth/admin/master/console/` returns `HTTP/1.1 200` + `Content-Type: text/html`. |
| Success | Both headers present. |

---

# WAVE 6 — Functional tests  (5-6 agents)

**Goal**: end-to-end Keycloak flows pass against the booted server.

## W6-A — `kcadm.sh` admin login

| | |
|---|---|
| Repro | `kcadm.sh config credentials --server http://localhost:8080/auth --realm master --user admin --password admin` exits 0 and stores a token in `~/.keycloak/kcadm.config`. |
| Success | Exit 0, config file populated. |

## W6-B — Create realm via REST

| | |
|---|---|
| Repro | `curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" -d '{"realm":"test","enabled":true}' http://localhost:8080/auth/admin/realms` returns 201. Then `curl -s http://localhost:8080/auth/realms/test` returns the realm. |
| Success | Both calls succeed. |

## W6-C — Create user via REST

| | |
|---|---|
| Repro | POST `/admin/realms/test/users` with `{"username":"alice","enabled":true,"credentials":[{"type":"password","value":"alice"}]}` returns 201. |
| Success | Subsequent `GET /admin/realms/test/users?username=alice` returns the user. |

## W6-D — OIDC password grant token flow

| | |
|---|---|
| Repro | POST `http://localhost:8080/auth/realms/test/protocol/openid-connect/token` with `grant_type=password&username=alice&password=alice&client_id=admin-cli` returns a JSON body with non-empty `access_token`. |
| Success | `access_token` field in response. |

## W6-E — JWKS endpoint

| | |
|---|---|
| Repro | `curl -s http://localhost:8080/auth/realms/test/protocol/openid-connect/certs \| jq -r '.keys[0].kty'` returns `RSA`. |
| Success | The string `RSA`. |

## W6-F — Single Keycloak integration test

| | |
|---|---|
| Repro | Pick the smallest Keycloak `testsuite/integration-arquillian` test class (e.g. `org.keycloak.testsuite.admin.realm.RealmTest::getRealms`). Run it via `mvn -pl testsuite/integration-arquillian test -Dtest=RealmTest#getRealms` against the rustjvm-booted Keycloak. |
| Success | Test passes. |

---

# WAVE 7 — Performance + soak  (3 agents, optional)

## W7-A — 24h soak with synthetic OIDC load

| | |
|---|---|
| Repro | Run `wrk -t4 -c100 -d24h http://localhost:8080/auth/realms/test/protocol/openid-connect/token --script=token-grant.lua` for 24 hours. |
| Success | Process alive, RSS within 2× initial, no FD leaks (`lsof` count stable), zero unhandled exceptions in server.log. |

## W7-B — Token throughput vs HotSpot

| | |
|---|---|
| Repro | `wrk -t4 -c100 -d60s` against HotSpot, then against rustjvm. |
| Success | rustjvm geomean within 2× HotSpot tokens/sec. |

## W7-C — Concurrent admin operations stress

| | |
|---|---|
| Repro | 100 concurrent realm-create + user-create + token-grant cycles. |
| Success | All complete; no NPE / lost updates / stuck threads. |

---

# Wave dependencies

```
W1 (visibility) -> W2 (MSC bootstrap) -> W3 (subsystems) -> W4 (deploy) -> W5 (HTTP) -> W6 (functional) -> W7 (perf/soak)
```

Within a wave, tasks are pairwise file-disjoint and parallel-safe.
The list under each wave is the dispatch slate.

# Estimated effort

| Wave | Agents | Per-agent effort | Wave wall-time at 4 parallel |
|---|---|---|---|
| W1 | 4 | 0.5–1d | 1d |
| W2 | 5 | 1–2d | 2-3d |
| W3 | 6 | 2–4d | 1-2 weeks (XNIO, Undertow are biggest) |
| W4 | 6 | 2–5d | 2-3 weeks (CDI, Hibernate are sprawling) |
| W5 | 4 | 0.5–1d (mostly verification) | 1-2d |
| W6 | 6 | 1–3d | 1-2 weeks |
| W7 | 3 | 1d each (mostly run-and-observe) | 3d |

**Realistic total**: 6-10 calendar weeks at 4-agent batches with the
operational discipline established in Sessions 96-101 (mandatory
strict-swallow trace, mandatory build+verify, ≤3 iteration timebox,
single-file scope, ≤4 agents per batch).

# How to dispatch a wave

1. Pick the next wave whose dependencies are all green.
2. For each task in the wave, draft a self-contained agent prompt
   following the Session 101 template (the one in
   [the kc16-blocker-map.md](./kc16-blocker-map.md) Session 101
   relaunch worked cleanly):
   - Mandatory step-numbered method
   - Each step has a required artifact in the report
   - 6-step report template enumerated explicitly
   - "If your final report does not include all 6 artifacts, you have
     not completed the task"
3. Dispatch ≤4 agents in parallel via the harness.
4. Wait for completions; merge usable patches; commit one session per
   wave with the same prose-sectioning style as Sessions 99-101.
5. Update [docs/kc16-test-roadmap.md](./kc16-test-roadmap.md) (this
   file) with the wave's outcome before dispatching the next wave.

# Defensive notes for future wave authors

- **Worktree-isolation drift**: harness sometimes branches a worktree
  from a stale commit. Every prompt must include the
  `git fetch && git merge` step at the top.
- **Watchdog timeout = 600s**: avoid commands that produce no stream
  output for >10 min. Long `cargo build`s are the main risk; prefer
  `cargo build --release -p rustjvm-cli` (the CLI crate alone) over a
  workspace-wide build.
- **Pre-existing apps/ paths**: `apps/dprop/HelloWorld` and
  `apps/annotated/AnnoTest` are reliable smoke fixtures that ship in
  the repo. Do not invent paths.
- **Investigation paralysis** is the dominant failure mode. The
  Session 100 first-attempt Agent A spent 28 minutes / 107 tool uses
  grepping and concluded "all properly implemented" without ever
  running the repro. The Session 101 relaunch with the strict
  6-step protocol shipped in 19 minutes. Mandate the empirical step.
- **Token-budget management**: 6+ simultaneous opus agents on
  multi-file investigations will exhaust account capacity (Session 100
  lesson — 5 of 6 stalled). Cap at 4. For trivial cleanup tasks
  (RJ.1-style) sonnet works.
