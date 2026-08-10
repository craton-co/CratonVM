# `HashMap.get` returns null for a key the same map's `entrySet()` just yielded — one-entry map, `==` key, equal `hashCode`

**Status: FIXED 2026-08-10.** Filed and closed the same day. The "open work"
this page names — *what condition the parked map hits that a generic `HashMap`
across a GC does not* — is answered in "The condition, found" at the bottom.
`HashMap` was never at fault: the key's identity hash **changed between `put`
and `get`**, because the key was first hashed while its own monitor was held.

Verified with this page's own reproducer, unchanged, on the fixed binary:

```
parked.get(service)     = len=1 [0@92574]     (was: null)
parked.containsKey      = true                (was: false)
re-get by entry key     = len=1 [0@92574]     (was: null)
```

which is what HotSpot prints on the same classpath.

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

## The condition, found

The first bullet above was the right instinct — *the hash used at `put` differed
from the hash used at `get`* — and this is the condition that makes it happen:

**the key was first hashed while its own monitor was held.**

`TomcatWebServer.initialize()` parks the connectors from inside the `Context`
`START_EVENT` listener, which runs inside `StandardService.startInternal()`,
which runs inside `LifecycleBase.start()` — and that method is
`public final synchronized void start()`, synchronized on the `StandardService`
that is about to become the map key. So the `put` happens with the key
THIN_LOCKED. By the time `addPreviouslyRemovedConnectors()` does the `get`, the
lock is long released.

CratonVM keeps the identity hash in the upper bits of a NEUTRAL mark word. A
`THIN_LOCKED` payload is an owner plus a recursion count and an `INFLATED`
payload is a monitor pointer, so neither has room for one; the accessor
correctly declined to decode them and returned `0`, and the VM's
"identityHashCode must never be 0" guard turned that `0` into `i32::MAX` and
handed it out. The `put` therefore filed the entry under `i32::MAX`, and the
`get` — by then NEUTRAL, so a real hash was minted — looked in a different
bucket.

That also explains the observation in "The invariant that breaks" that
`hashCode()` reads the same 92563 from both references *now*: it does. The
divergence is between insertion time and now, exactly as this page suspected,
and reading the hash twice after the fact can never show it.

Two of the recorded refutations are now explained rather than merely refuted:
the `HashMap<Service, Connector[]>` round-trip in `TomcatFindConnectorsProbe`
passed because it never took the key's lock, and `IdentityHashAcrossGcProbe`
found zero misses across 200 keys and ~900MB of churn because a moving GC was
never the mechanism. The generic version could not reproduce because the
condition is not "a HashMap across a GC" but "hash it inside `synchronized`".

Three lines are enough (`probes/IdentityHashWhileLockedProbe.java`):

```java
Object p = new Object();
synchronized (p) { inside = System.identityHashCode(p); }
afterUnlock = System.identityHashCode(p);
```

| | HotSpot | before | after |
|---|---|---|---|
| `inside` / `afterUnlock` | equal | `2147483647` then `16` | equal |
| `HashMap.put` under the key's own lock, then `put` again | size 1 | size 2 | size 1 |

**Fix:** `MonitorTable::identity_hash_via_monitor` inflates and displaces the
hash into the monitor, which is what HotSpot's
`ObjectSynchronizer::FastHashCode` does for a stack-locked object, and it is
stable for the object's life because nothing here deflates a live object's
monitor. Fences in `vm/src/threading/monitor.rs`:
`the_identity_hash_of_a_thin_locked_object_survives_the_unlock` and
`locked_objects_do_not_all_share_one_identity_hash` — both verified red against
the pre-fix code (`left: 2147483647, right: 2147483647`) before being trusted.

**Blast radius, answered.** Two shapes, both of them ordinary Java:

1. Any `HashMap`/`HashSet` keyed on an object whose hash was first taken inside
   its own `synchronized` block loses the entry.
2. Every locked-then-hashed object in the process shared the single value
   `i32::MAX`, so such keys also collided with each other.

Measured effect beyond this class: the SSL/PEM/JKS + http-client cluster went
17 FAIL → 1 (see
[`ssl-pem-jks-and-http-client-cluster-20260809-FIXED.md`](ssl-pem-jks-and-http-client-cluster-20260809-FIXED.md),
where this is defect 2 of 3), and a twelve-class embedded-server sample went
from "every class reports port 8080" to **zero** occurrences.
