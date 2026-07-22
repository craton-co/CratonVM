# Bug TC0622 — `MBeanServer.queryNames` returns an empty Set on CratonVM's synthetic platform MBeanServer (registered Tomcat MBeans invisible to JMX queries)

> ## ✅ FIXED (2026-06-23) — branch `fix/tc0622-jmx-query`, merged to `dev`
>
> Root cause was **three** independent defects in the synthetic MBeanServer
> (`native-builtins/src/jmx.rs`), all required:
> 1. **Fake result Set** — `queryNames`/`queryMBeans` built a synthetic 2-field
>    `java.util.HashSet` stand-in whose real `.size()`/`iterator()` read its
>    (empty) backing map → always 0 (the count↔query smoking gun). Now builds a
>    **real** `HashSet` via `new` + `add()`.
> 2. **No pattern matching** — queries ignored the `ObjectName` pattern and
>    returned all names. Now filtered via the real `ObjectName.apply()` bytecode
>    (`Catalina:*`≠`Tomcat:*`, `java.lang:*` MXBeans excluded).
> 3. **Canonical-name reconstruction** — registry stored only sorted canonical
>    key strings; reconstructing via `getInstance` changed `toString()` key
>    order and never matched the test's expected strings. Now stores/returns the
>    **original `ObjectName` objects** (new `MBS_ONAMES` slot).
>
> Plus a `DynamicMBean.getAttribute(String)` delegation path (Tomcat modeler
> `BaseModelMBean`). All heap walks GC-safe (pinned accumulator/candidate).
>
> **Validation:** `org.apache.catalina.mbeans.TestRegistration` → `OK (2 tests)`
> on CratonVM == HotSpot (via `run-suite.ps1`); standalone probe matches HotSpot
> (queryNames `Tomcat:*`=2 in correct key order, `Catalina:*`=0, queryMBeans=2,
> real-`HashSet` `remove()` works); 52/52 native-builtins `jmx` unit tests green.
>
> **`findMBeanServer(null)`** also fixed (follow-up commit): the synthetic
> `createMBeanServer` now appends the server to the real
> `MBeanServerFactory.mBeanServerList` so the un-overridden real
> `findMBeanServer` bytecode reports it (`newMBeanServer` left untracked, per the
> JMX contract). `find(null)` == HotSpot: `0 → 1` (same instance as
> `getPlatformMBeanServer`) `→ 2` after another `createMBeanServer`, unchanged by
> `newMBeanServer`.
>
> **Architecture note:** the real `com.sun.jmx.mbeanserver.JmxMBeanServer`
> classes *are* loadable on CratonVM, so dropping the `createMBeanServer`
> override and running the real server end-to-end is feasible and more faithful,
> but has broad startup blast radius (Kafka/WildFly/Spring) — deferred.

> **Root cause:** Under the default-on `experimental-jmx` feature, CratonVM
> overrides `javax/management/MBeanServerFactory.createMBeanServer()` to return a
> *synthetic* `javax/management/MBeanServer` (an in-process registry on the heap,
> `native-builtins/src/jmx.rs::register_mbean_server` / `alloc_mbean_server`)
> instead of letting the real JDK build a `com.sun.jmx.mbeanserver.JmxMBeanServer`.
> Registration "works" (`getMBeanCount` / `isRegistered` see the beans), but the
> synthetic server's **`queryNames(ObjectName,QueryExp)` and `queryMBeans(...)`
> natives return an empty Set regardless of what is registered**, so any code that
> enumerates beans by pattern (Tomcat's test, JConsole, `Registry`) sees nothing.

**Severity:** Medium (breaks all JMX bean *enumeration*; registration itself is
unaffected, so it does not block Tomcat startup — only management/monitoring and
the registration-assertion tests fail). Broad blast radius for any JMX-querying
app, not Tomcat-specific.

**Status on CratonVM:** FAIL. **HotSpot:** PASS.
**Run date:** 2026-06-23
**Binary:** dev `df11ac00` (run via worktree `C:\craton\CratonVM-tctest`,
exe `target/release/cratonvm-tcfull-0622.exe`).

**Affected classes (1 observed; whole JMX subsystem implicated):**
`org.apache.catalina.mbeans.TestRegistration` (method `testMBeanDeregistration`).
Any application that calls `MBeanServer.queryNames` / `queryMBeans` after
registering MBeans is affected the same way.

## Symptom

After embedded Tomcat starts, `TestRegistration` asserts a known set of ~30 JMX
`ObjectName`s are registered in the platform MBeanServer
(`Registry.getRegistry(null).getMBeanServer().queryNames(new ObjectName("Tomcat:*"), null)`).
On CratonVM the query returns **none** of them:

```
java.lang.AssertionError: Missing Tomcat MBeans: [Tomcat:type=Engine,
  Tomcat:type=Realm,realmPath=/realm0, Tomcat:type=Mapper, Tomcat:type=MBeanFactory,
  Tomcat:type=NamingResources, Tomcat:type=Server, Tomcat:type=Service,
  Tomcat:type=StringCache, Tomcat:type=UtilityExecutor,
  Tomcat:type=Valve,name=StandardEngineValve, ... ~30 ObjectNames ]
        at org.apache.catalina.mbeans.TestRegistration.testMBeanDeregistration(TestRegistration.java:175)
```

Tomcat boots cleanly to "Starting/Stopping service [Tomcat]" — the failure is the
post-startup MBean *enumeration*, not the lifecycle.

The defect is NOT "registration is a no-op". A minimal probe shows registration
genuinely lands in the synthetic registry; only the *query* path is empty:

```
# CratonVM (cratonvm-tcfull-0622.exe)               # HotSpot (jdk-25)
server class = javax.management.MBeanServer          server class = com.sun.jmx.mbeanserver.JmxMBeanServer
isRegistered = true                                  isRegistered = true
count initial = 12, +1 = 13, +2 = 14                 mbeanCount   = 29 (grows on register)
queryNames("Tomcat:*") size = 0                      queryNames("Tomcat:*") size = 1  -> Tomcat:type=Engine
queryNames(null,null)  size = 0                      queryNames(null,null)  size = 29
queryNames("*:*")      size = 0                      queryNames("*:*")      size = 29
findMBeanServer(null)  size = 0                      findMBeanServer(null)  size = 1
```

(Probe: `getPlatformMBeanServer()`, register a trivial `FooMBean` under
`Tomcat:type=Engine`, then query. Same server instance returned on repeated
`getPlatformMBeanServer()` calls — `==` is true — so this is not a fresh-server
race.)

## Root cause (analysis)

Two facts combine, both inside CratonVM's synthetic JMX implementation
(`native-builtins/src/jmx.rs`), which is compiled in by default because
`experimental-jmx` is in the **default feature set** (`vm/Cargo.toml:33`;
`vm/src/vm/vm_init.rs:1451-1452` calls `register_jmx_natives` under that cfg).

1. **The platform server is synthetic, not the real `JmxMBeanServer`.**
   `getPlatformMBeanServer()` is deliberately left unregistered so the real JDK
   bytecode runs (the long "KAFKA-MBEAN" note at `jmx.rs:990-1007` and
   `vm_exec.rs:9917-9927` explains this avoids an `AbstractMethodError` on
   interface dispatch). But that bytecode calls
   `MBeanServerFactory.createMBeanServer()`, and CratonVM *does* override
   `createMBeanServer`/`newMBeanServer` (`jmx.rs:2444-2466`, `make_server_fn` →
   `alloc_mbean_server`). So the "real path" still bottoms out in a synthetic
   `javax/management/MBeanServer` whose registry is four heap slots
   (`MBS_DOMAIN/COUNT/NAMES/BEANS`, `jmx.rs:1789-1808`). The probe confirms
   `server class = javax.management.MBeanServer` (the interface), proving the
   real `com.sun.jmx.mbeanserver.JmxMBeanServer` is never built.

2. **`queryNames`/`queryMBeans` on that synthetic server return empty.** The
   query natives (`jmx.rs:2146-2217`) read the `MBS_NAMES`/`MBS_BEANS` parallel
   arrays via `mbs_registry`. Yet `getMBeanCount` reports a *non-empty, growing*
   count (12 → 13 → 14) for the very same instance, while `queryNames` over the
   same instance is 0 for every pattern (`"Tomcat:*"`, `null`, `"*:*"`). That
   internal inconsistency — count and `isRegistered` see the beans, the query
   natives do not — means the registry the registration path populated is not the
   registry the query natives read back (or the query natives are dispatching to a
   different/real-bytecode body while register/count hit the synthetic natives).
   The synthetic query implementation never parses the `ObjectName` pattern
   either (`jmx.rs:2139-2145` comment: "null/empty pattern means all"), so even a
   working registry array would not do pattern matching — but here it returns
   empty even for the all-match `null` pattern.

   Secondary, same family: `MBeanServerFactory.findMBeanServer(null)` returns an
   empty list (HotSpot: 1) because the synthetic `createMBeanServer` override does
   not register the created server in the real factory's static server list that
   `findMBeanServer` consults (`findMBeanServer` is *not* overridden, so it runs
   real bytecode over an empty real registry). Tomcat's
   `Registry.getMBeanServer()` (`apps/tomcat/java/org/apache/tomcat/util/modeler/Registry.java:445-456`)
   therefore always falls through to `getPlatformMBeanServer()`. That fallback
   still yields the synthetic server, so it is not the proximate cause of the
   assertion, but it is a second divergence in the same synthetic-factory area.

**Pinned area:** `native-builtins/src/jmx.rs` —
`register_mbean_server` (the `queryNames`/`queryMBeans` natives at lines
2146-2217, the registry helpers `mbs_registry`/`mbs_find` at 1838-1866, and the
`MBeanServerFactory.createMBeanServer`/`findMBeanServer` override block at
2434-2466). Gated by `experimental-jmx`, default-on
(`vm/Cargo.toml:33`, wired at `vm/src/vm/vm_init.rs:1451`).

## Reproduction

Single-class re-run (same jvm args `run-suite.ps1` uses):

```powershell
cd C:\craton\CratonVM\apps\tomcat
$CP = (Get-Content .tooling\cp.txt -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS=1; $env:CRATONVM_REAL_AQS=1; $env:CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe -cp $CP `
  -Dtomcat.test.basedir=C:\craton\CratonVM\apps\tomcat\output\build `
  -Dtomcat.test.tomcatbuild=C:\craton\CratonVM\apps\tomcat\output\build `
  -Dtomcat.test.temp=C:\craton\CratonVM\apps\tomcat\output\test-tmp `
  --add-opens java.base/java.lang=ALL-UNNAMED --add-opens java.base/java.util=ALL-UNNAMED `
  org.junit.runner.JUnitCore org.apache.catalina.mbeans.TestRegistration
```

Minimal standalone repro (no Tomcat) — register one MBean, query it back:

```java
MBeanServer s = ManagementFactory.getPlatformMBeanServer();
s.registerMBean(new Foo() /* implements FooMBean */, new ObjectName("Tomcat:type=Engine"));
System.out.println(s.getClass().getName());                    // craton: javax.management.MBeanServer ; hotspot: ...JmxMBeanServer
System.out.println(s.isRegistered(new ObjectName("Tomcat:type=Engine"))); // both: true
System.out.println(s.queryNames(new ObjectName("Tomcat:*"), null).size()); // craton: 0 ; hotspot: 1
```

## Recommendation

**HANDOFF (not an inline fix in this triage).** Scope reasoning:

- This is a **subsystem gap**, not a one-line plumbing bug. CratonVM ships a
  *partial* synthetic JMX agent under `experimental-jmx` whose registry,
  `ObjectName`-pattern matching, query, and factory-bookkeeping are all
  incomplete. Making `queryNames`/`queryMBeans` honor the registry would close
  *this* test, but `ObjectName` wildcard/property-pattern semantics, `QueryExp`
  evaluation, `findMBeanServer` server tracking, and `MBeanInfo`/StandardMBean
  attribute introspection are all part of the same surface the
  `TestRegistration` family (and real JMX clients) exercise. A correct fix should
  decide between (a) finishing the synthetic agent's query/registry plumbing so
  it is self-consistent, or (b) stopping the `createMBeanServer` override and
  letting the real JDK `com.sun.jmx.mbeanserver.JmxMBeanServer` run end-to-end
  (which is what the "KAFKA-MBEAN" note already prefers for `registerMBean`
  interface dispatch) — the latter is the more faithful path but needs the real
  `java.management` MBeanServer chain to interpret on CratonVM without tripping
  the interface-dispatch/AbstractMethodError problem the note describes.
- **Lowest-risk first step** (if a targeted fix is wanted): make the synthetic
  `queryNames`/`queryMBeans` natives actually read the same `MBS_NAMES` array the
  register path grows AND implement `ObjectName.apply`-style pattern matching
  (domain + key-property subset, including the `*`/`?` wildcards and `:*` "all
  keys" form). Verify the count↔query consistency (today count=14 but query=0 on
  one instance) — that inconsistency is the concrete smoking gun and the first
  thing to chase. Re-run the standalone repro until `queryNames` matches
  `getMBeanCount`, then the full `org.apache.catalina.mbeans.TestRegistration`.
- Because the implementation is gated by the default-on `experimental-jmx`
  feature, a regression-safe alternative for unblocking startup-only suites is to
  note that disabling that feature reverts to the (also-incomplete) real-JDK
  path; not recommended as a fix, but useful to bisect which path a given app
  needs.
