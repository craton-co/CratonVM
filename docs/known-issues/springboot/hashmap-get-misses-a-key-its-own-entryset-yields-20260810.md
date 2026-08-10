# `HashMap.get` returns null for a key the same map's `entrySet()` just yielded — one-entry map, `==` key, equal `hashCode`

**Status: OPEN — CratonVM correctness bug, root-caused to a minimal reproducer. Filed 2026-08-10.**

This is the root cause of 82 of the 90 test failures in
`TomcatServletWebServerFactoryTests`, which had been on file as a throughput
problem (`tomcat-jetty-servletwebserverfactorytests-300s-budget-overrun-20260807.md`).
It is a general `HashMap` defect, not a Tomcat or Spring one; that class is
simply where it was caught.

## The invariant that breaks

```
parked map size           = 1
  service id              = 92563
  parked.get(service)     = null
  parked.containsKey      = false
  entry key id            = 92563 value len=1 [0@92578]
    key == service        = true
    key.equals(service)   = true
    key.hashCode()        = 92563
    service.hashCode()    = 92563
    identityHashCode(key) = 92563
    re-get by entry key   = null
```

A `HashMap` with **one** entry. The key in that entry is reference-identical to
the key being looked up (`==` is true), `equals` agrees, and `hashCode()` is the
same value read from both references. `get` and `containsKey` both miss — and
so does `get(e.getKey())` where `e` came from that map's own `entrySet()`.

No key-mutation story is needed to state the bug: **a map whose `entrySet()`
yields entry E must not return null for `get(E.getKey())`.** Whatever bucket
the entry was filed under at `put` time is not the bucket the current hash
selects, so the table is internally inconsistent with its own contents.

Note what the numbers rule out. `hashCode()` is stable *now* — both references
report 92563 — so this is not "the hash changed between the two reads". The
inconsistency is between insertion and the present, which points at either the
hash used at `put` time or at whatever moved the table afterwards.

HotSpot 25 on the identical classpath and code path: `parked.get(service)`
returns `len=1 [0@…]`, the connector is restored, the server binds an ephemeral
port.

## Reproducer

`probes/springtomcat/ParkedConnectorsProbe.java`. Runs in about ten seconds, no
suite harness:

```
javac -cp "$(cat apps/spring-boot/module/spring-boot-tomcat/build/cratonvm-test-cp.txt)" \
      -d <out> probes/springtomcat/ParkedConnectorsProbe.java
<cratonvm> --java-home <jdk25> -cp "<out>;<that same cp>" \
      org.springframework.boot.tomcat.ParkedConnectorsProbe
```

It lives in package `org.springframework.boot.tomcat` because
`TomcatWebServer.getServiceConnectors()` is package-private, and reading the
real parked map is the whole point — every synthetic stand-in tried first came
back identical between the two VMs (see "Refuted" below).

## Why it produces a server on port 8080

Spring Boot's `TomcatWebServer.initialize()` removes every connector from every
`Service` and parks them in a `Map<Service, Connector[]>`, so nothing binds a
port before `start()`. `start()` calls `addPreviouslyRemovedConnectors()`,
which iterates `getServer().findServices()` and looks each Service up in that
map.

When that lookup misses, the connectors are never restored — and the next line
is `this.tomcat.getConnector()`, which is **not** a getter. With no connector
present, Tomcat *fabricates* one on port 8080 and adds it to the Service. So a
lost map entry does not surface as "no server". It surfaces as "a server on the
wrong, fixed port", which then collides with anything already holding 8080 and
fails the whole start with
`ConnectorStartFailedException: Connector configured to listen on port 8080
failed to start`.

That is why the failure looks like a port conflict and is not one.

Observed directly in `probes/SpringFactoryPortProbe.java`:

| | HotSpot 25 | CratonVM |
|---|---|---|
| after `getWebServer()` | connectors=0 | connectors=0 |
| after `start()` | connectors=1, port 0 → bound **49418** | fabricates `http-nio-8080`, fails |
| `ProtocolHandler` names | — | `http-nio-auto-1` *then* `http-nio-8080` |

Both VMs agree the connectors are removed. Only the restore differs.

## Refuted — each with a paired probe, each identical between the two VMs

These are recorded so the next person does not re-run them:

* **Field-initializer ordering across a constructor chain**
  (`probes/FieldInitVsSuperCtorProbe.java`). `AbstractConfigurableWebServerFactory`
  has `private int port = 8080` plus a constructor assigning `this.port`, two
  subclasses below the factory the tests build — a textbook JLS 12.5 trap.
  Identical, all twelve rows.
* **`Connector.setPort` → ProtocolHandler propagation**
  (`probes/TomcatConnectorPortProbe.java`). Tomcat forwards the port by *name*
  through `IntrospectionUtils.setProperty`, and by-name writes that silently
  no-op are a known CratonVM failure family. Identical at all three levels
  (setter return, `Connector.getPort()`, handler readback).
* **`StandardService.addConnector` / `findConnectors()` and the
  `Tomcat.getConnector()` fallback** (`probes/TomcatFindConnectorsProbe.java`).
  Identical, including array growth and removal.
* **`Service` identity: `getService()` vs `getServer().findServices()[0]`**
  (same probe). Same object on both VMs, and a `HashMap<Service, Connector[]>`
  park/restore round-trip on those very objects succeeds on both.
* **Identity-hash stability and `HashMap`/`IdentityHashMap` survival across GC**
  (`probes/IdentityHashAcrossGcProbe.java`), 200 keys, ~900MB of churn, two
  collection rounds. Zero hash changes and zero lookup misses on both VMs.

The last one matters most: the generic version of this bug does **not**
reproduce. Whatever condition the parked map hits is more specific than "a
HashMap across a GC", and finding it is the open work.

## Not yet established

* The mechanism. A one-entry map missing its only key is consistent with the
  hash used at `put` differing from the hash used at `get`, and with table
  corruption; nothing here distinguishes them. Instrumenting
  `HashMap.putVal`/`getNode` to log `(key identity, hash, bucket index)` on both
  paths would, in one run.
* Whether `java.util.HashMap` runs as real JDK bytecode here or hits a
  CratonVM-side intrinsic/native — that decides which layer to instrument, and
  it was not checked.
* Blast radius. Any `HashMap` keyed on objects that do not override
  `hashCode` is exposed, which is a large surface across the suites. No census
  was taken.
