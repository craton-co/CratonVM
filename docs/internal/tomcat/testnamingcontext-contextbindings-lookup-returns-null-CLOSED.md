# `TestNamingContext` — the binding was filed under one identity hash and looked up under another

| | |
|---|---|
| **Status** | ✅ **CLOSED 2026-08-10** — root-caused; already fixed on `dev` by [`67fadfdd8`](#the-fix-that-closed-it) (2026-08-10 01:25) |
| **HotSpot** | PASS (`OK (2)`) |
| **CratonVM** | was FAIL 2/2 on all 3 GC backends; now `OK (2)` on default / G1 / ZGC |
| **Fix** | `67fadfdd8` *"an object hashed while locked kept that hash after the unlock"* — landed BEFORE this page was written, in a commit this page did not know about |
| **Discovered** | 2026-08-10, cross-referencing FAILs common to all three GC-backend full-suite runs |

## Symptom

```
1) testGlobalNaming(org.apache.naming.TestNamingContext)
java.lang.NullPointerException: Cannot invoke "javax.naming.Context.lookup(String)" because "context" is null
	at org.apache.naming.TestNamingContext.doLookup(TestNamingContext.java:153)
2) testModuleEquivalentToComp  (same shape)
```

`ContextBindings.getContext(ctx)` returned `null` for a catalina `Context` whose
binding `tomcat.start()` had just registered.

## Root cause

`ContextBindings` keys its registry by object identity:

```java
private static final Map<Object,Context> objectBindings = new ConcurrentHashMap<>();
static Context getContext(Object obj) { return objectBindings.get(obj); }
```

The `put` side runs from `NamingContextListener.lifecycleEvent` on
`CONFIGURE_START_EVENT`:

```java
ContextBindings.bindContext(container, namingContext, token);   // -> objectBindings.put(container, …)
```

`CONFIGURE_START_EVENT` is fired from `StandardContext.startInternal()`, which
runs inside `LifecycleBase.start()` — and that method is

```java
public final synchronized void start() throws LifecycleException {
```

i.e. **synchronized on the very `StandardContext` used as the map key.** So the
key is hashed while its own monitor is held, and read back after the monitor is
released.

Before `67fadfdd8`, `Object.hashCode()` on a `THIN_LOCKED` or `INFLATED` object
answered `i32::MAX` and a different, freshly minted value once the mark word
went back to `NEUTRAL`. The mark word carries the identity hash only in the
`NEUTRAL` state; `mark_word_identity_hash` correctly refused to decode a lock
payload as a hash, and the displaced-hash hook answered `0` for an object never
hashed before it locked — which the "identity hash is never 0" guard turned into
`Integer.MAX_VALUE`.

The entry was therefore filed under `Integer.MAX_VALUE` and looked up under the
object's real hash. Nothing was lost and nothing was collected: the map simply
could not find its own entry.

Measured with `probes/IdentityHashWhileLockedProbe.java` (three lines of Java),
on the binary the failing suite runs used vs. today's `dev`:

| | HotSpot | pre-fix binary | post-fix binary |
|---|---|---|---|
| `identityHashCode` inside `synchronized`, then after | equal | `2147483647` then `16` | equal |
| `HashMap.put` under the key's own lock, then `put` again → `size()` | 1 | **2** | 1 |

## Why the page said OPEN when the fix had already landed

The three GC-variant full-suite runs this page was written from
(`fullsuite-gc{default,g1,zgc}-2shard-20260810`, and the earlier
`fullsuite-4shard-20260807`) all ran a binary built from
`fix/g1-fullsuite-regression-20260809` at 06:20 on 2026-08-10. That branch had
not merged `dev` since before `67fadfdd8` (01:25) — `git merge-base
--is-ancestor 67fadfdd8 <branch>` is false for it. `dev` itself was already
green.

The page also asserted a standalone reproduction it had not run. Standalone on
`dev`, the class passes; **on the same platform, with the older binary, it fails
3/3**. Running the older binary rather than re-running the newer one is what
turned "this needs a root-cause investigation" into a five-hour bisect window and
then into a named commit.

The page's own suspicion ("a key-identity or registration-ordering gap … given
the family of prior identity/`ThreadLocal`-keyed lookup bugs") named the right
family. It just did not know the family already had a fix.

## Verification

`apps/tomcat-suite-runner/run-one.ps1`, suite launch environment:

| binary | result |
|---|---|
| `CratonVM-g1reg-20260809/target/release/cratonvm-g1fix-20260810.exe` (pre-fix) | FAIL 2/2 — **3 runs of 3** |
| `cratonvm-elnaming-fix1-20260810.exe` (dev `845a1ca75`) | `OK (2)` — **3 runs of 3** |

and across collectors on the post-fix binary: default `OK (2)`, `-XX:+UseG1GC`
`OK (2)`, `-XX:+UseZGC` `OK (2)`. HotSpot control `OK (2)` in 1.1 s.

`CRATONVM_DBG_GC_STRESS=65536` also passes, which is worth recording: the page
framed the defect as possibly GC-related because it reproduced on all three
collectors. It reproduced on all three because the mark word is collector-
independent, not because any collector moved anything.

## Regression pin

`67fadfdd8` landed with a `probes/` reproducer, which nothing schedules. This
page's retirement adds `regression-suite/src/RLockedIdentityHash.java` to
`CORE_CLASSES` so the invariant is checked on every suite run and diffed against
HotSpot: hash-before-lock, first-hash-under-lock, three-deep recursive entry, an
INFLATED monitor owned by another thread, 64 distinct locked objects (the
failure gave them all ONE hash, so "unstable" alone would not have caught it),
and the `put`-under-the-key's-own-lock shape in `HashMap`, `ConcurrentHashMap`
and `IdentityHashMap`.

RED-then-GREEN, verified against real binaries rather than reasoned about:

| binary | `RLockedIdentityHash` |
|---|---|
| `cratonvm-g1fix-20260810.exe` (pre-`67fadfdd8`) | **FAIL** (`AssertionError`) |
| `cratonvm-elnaming-fix1-20260810.exe` (dev, post-fix) | PASS, output byte-identical to HotSpot |

Hash **values** are never printed — they are legitimately VM-specific, and a
printed one would make the runner's HotSpot diff fail for a correct VM. Only
stability booleans, a "not all 64 share one hash" count, and map sizes.

## Sibling witness

Spring Boot's `TomcatWebServer` hits the identical shape — a
`Map<Service,Connector[]>` written from inside `LifecycleBase.start()`, which is
`synchronized` on that `StandardService`. There the missed lookup made
`Tomcat.getConnector()` fabricate a default port-8080 connector for an
already-running service, and every embedded-Tomcat test failed with
`Connector configured to listen on port 8080 failed to start` — a port conflict
that was not one. Same defect, same commit, unrecognisably different face.
