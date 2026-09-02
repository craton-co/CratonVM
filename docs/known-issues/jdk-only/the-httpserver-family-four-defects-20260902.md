# The HttpServer family: four defects, and the one I published before checking

**Status: MEASURED and FIXED 2026-09-02** on `azure-host-2`
(`azureuser@20.80.105.49`). Every number below was taken on a binary built from
this tree, on all three CratonVM arms, against HotSpot 25 on the same host.
**VERIFIED AGAINST A BINARY 2026-09-02.**

**Lane** L7. **Files** `native-builtins/src/net_phase_e.rs`,
`native-builtins/src/phases_late/net_channels.rs`.
**Probes** `probes/HttpServerFactoryDoorProbe.java`,
`probes/HttpContextCarrierProbe.java`.

---

## 0. Start here: a claim I made and then refuted

[`the-ratchet-that-saw-five-of-sixty-one-20260902.md`](the-ratchet-that-saw-five-of-sixty-one-20260902.md)
§5 said nine instance-method registrations on `com/sun/net/httpserver/HttpServer`
were unreachable, because the abstract class cannot be a receiver. **That is
wrong.** The census, taken through the other factory:

```text
com/sun/net/httpserver/HttpServer   11 rows, 11 invocations   <- no-arg create()
sun/net/httpserver/HttpServerImpl   11 rows,  1 invocation    <- two-arg create()
```

`HttpServerWildcardAddressProbe` only ever calls `create(InetSocketAddress, int)`.
Every zero I read was a fact about the door my probe took, published as a fact
about the class.

The general rule is right — a registration on an abstract class is unreachable
**unless this VM mints a carrier under that exact name** — and the exception is
what applies here: `re10_create_unbound_server` mints exactly that name, with a
comment at the mint site saying so. One `grep` for the class name as a literal
would have found it. I then compounded it by reading `HttpServerImpl`'s rows as
independently-authored twins; they are an `alias_class` SNAPSHOT of the public
class's rows, so "two classes with the same eleven methods" was one author, not
two.

**Chasing the wrong claim is what found the four real defects below.** That is
not a defence of publishing it.

## 1. `create()` returned an instance of an ABSTRACT class

```text
HotSpot     HttpServer.create() -> sun.net.httpserver.HttpServerImpl
CratonVM    HttpServer.create() -> com.sun.net.httpserver.HttpServer     ABSTRACT
```

`getClass().getName()` answered a class no JDK can instantiate; a cast or
`instanceof` against the impl type could not match; and it survived `--jdk-only`,
whose whole promise is that real class bytes are authoritative.

**The instrument had already said so, and nobody had read it.** The
uninstantiable-receiver census prints, on every affected run:

```text
a native allocated an instance of a class `new` could not produce (JVMS 6.5)
  class=com/sun/net/httpserver/HttpServer
  requester=native-builtins/src/net_phase_e.rs:20741 kind="abstract"
```

**Fix:** mint `HS_IMPL_CLASS`, as the two-arg factory already did.

The code minted the public name deliberately, to reach phase-72 rows registered
after the `alias_class` snapshot. That reason no longer held: all eleven
`HttpServer` rows are `registered_by = net_phase_e.rs` with `overwrote = null`,
and phase 72 runs AFTER phase E — had it registered those keys it would own the
slots. The alias copies the complete surface.

## 2. Phase 72's impl-class rows named a class that exists in no JDK

```rust
let hs_simple = "com/sun/net/httpserver/HttpServerImpl";   // com.sun.* -- wrong
```

The JDK's class is `sun.net.httpserver.HttpServerImpl`, which is what
`HS_IMPL_CLASS` spells and what both factories mint. **Every row registered on
that spelling was unreachable from the day it was written.**

`class_manager.rs` had already found this same typo in its field-count table and
fixed it there, by listing both spellings, with a comment explaining the package
error. The REGISTRATION half was left behind — a fix applied at one of its two
sites, which is how a defect survives its own discovery.

## 3. `createContext()` returned a context whose every accessor threw

The severe one.

```text
                 HotSpot              CratonVM compatible / --jdk-only
context class    HttpContextImpl      com.sun.net.httpserver.HttpContext
getPath          /probe               ! AbstractMethodError: ... has no Code attribute
getServer        true                 ! AbstractMethodError
getHandler       true                 ! AbstractMethodError
getAttributes    true                 ! AbstractMethodError
```

Not a wrong answer — no answer. `HttpServer.createContext(path, handler)` handed
back an object on which every documented method failed, on **both shipping
arms**.

**Why it survived:** the accessors were registered only in
`register_p72_http_server`, and phase 72 is reached solely from
`register_synthetic_overrides`. So the surface existed under `--synthetic-jdk`
and nowhere else, while `createContext` minted the carrier in every mode.
Native dispatch is what gives an abstract carrier its behaviour; with no rows
registered, every call resolved to the abstract declaration instead. **Anyone
who tested HttpContext under synthetic mode saw it work.**

**Fix:** the surface moved to `register_http_context_surface`, called from BOTH
phase 72 and phase E (which runs in all modes), registered on both class names.
Shared rather than copied: a duplicated 120-line block is how this repo ended up
with four copies of one search rule.

## 4. `getServer()` was null in every mode

Phase 72's ONE-arg `createContext` recorded the owning server in the link table.
The TWO-arg overload — the one javac emits for `createContext(path, handler)`,
and the only one registered in a real-JDK build — never did, so the documented
`context.getServer().getExecutor()` idiom saw null everywhere.

This was found only because the probe asks each accessor SEPARATELY: `getServer`
was false before the §3 fix and still false after, which is what identified it as
a distinct defect rather than a regression introduced by that fix. A probe that
stopped at the first failure would have reported its own reach.

## 5. After

All three arms, fresh binaries:

```text
                       compatible   --jdk-only   --synthetic-jdk   HotSpot
no-arg create() class  ImplClass    ImplClass    ImplClass         ImplClass
two-arg create() class ImplClass    ImplClass    ImplClass         ImplClass
getPath                /probe       /probe       /probe            /probe
getServer              true         true         true              true
getHandler             true         true         true              true
getAttributes          true         true         true              true
```

The uninstantiable census no longer names `com/sun/net/httpserver/HttpServer`.

**Two traps this run walked into, both recoverable only because they were
checked:**

* **The synthetic arm was measured on a stale binary for three rounds.** The test
  script hardcodes two binaries and only one was rebuilt, so every
  `--synthetic-jdk` line — including one that looked like confirmation — came
  from a binary predating the fixes. Rebuild every binary a script names.
* **Absent from a compat dump is not de-registered.** The evidence that §1's
  stated blocker "does not exist" came from compat/real-jdk/jdk-only censuses,
  and phase 72 appears in NONE of them by construction. Under `--synthetic-jdk`
  those rows are real, which is why §2 had to be fixed for §1 to be safe.

## 6. Left open, deliberately

`HttpContext` is still minted under the abstract public name. Now that rows exist
on both spellings, changing the mint would make `getClass()` match HotSpot — but
unlike `HttpServerImpl`, **nothing in the tree mints `HttpContextImpl` today**, so
there is no precedent that the real class's field layout will not clash with a
2-slot carrier in real-JDK mode. Every accessor answers through natives, so what
remains is the reported name. Recorded rather than shipped untested.

`HttpExchange` is abstract and minted too, at `net_phase_e.rs`. The census never
named it because these probes never serve a request — **the census reports only
what a run reaches**, so its silence about a class is not a clearance.

## Gate note

`raw_lock_constructions_do_not_grow` is RED at 429 against a baseline of 428, and
it is **not this branch's**: replicating the ratchet's own counting over HEAD and
the working tree gives 429 for both, a delta of 0, and 429 holds across the last
fourteen commits on `dev`. The baseline is stale. Not raised here — the test says
not to, and it is right.

## Reproduce

```bash
source /data/toolchain/env.sh
cargo build -p cratonvm-cli                              # real-JDK arms
cargo build -p cratonvm-cli --features synthetic-jdk     # and this one, every time
javac -d /tmp/p probes/HttpServerFactoryDoorProbe.java probes/HttpContextCarrierProbe.java
cd /tmp/p
for arm in "" --jdk-only --real-jdk; do
  cratonvm --java-home $JAVA_HOME $arm -cp . HttpContextCarrierProbe
done
$JAVA_HOME/bin/java -cp . HttpContextCarrierProbe        # the oracle
```
