# The permission family's constructors never ran their real `init` — FIXED 2026-08-29

## Status

**FIXED.** `RPermissionInit` (32 checks, oracle-matched against HotSpot) guards
it; regression-suite 78/78.

## Symptom

Every `java.security.Permission` subclass this VM built came out inert:

```
new PropertyPermission("a.b.*", "read,write")
  HotSpot   mask=3  path="a.b.*"  getActions()="read,write"
  CratonVM  mask=0  path=null     getActions()=""
```

Consequences, all measured against HotSpot:

* `implies()` threw `NullPointerException: Cannot invoke "String.equals(Object)"
  because "this.path" is null`
* `new PropertyPermission(k, "bogus")` was **accepted**, where the JDK throws
  `IllegalArgumentException` — a permission object with no parsed actions
* `getActions()` answered `""` for every input
* a serialization round trip died in `readObject`'s `init(getMask(actions))`
  with `IllegalArgumentException: invalid actions mask`

The last of those is how it surfaced, which is the symptom furthest from the
cause: `readObject` re-runs `init(getMask(actions))` itself, so it failed on an
`actions` that had been serialized as `""`.

## Root cause

`register_essential_natives` installs ONE closure over the whole family —
`Permission`, `BasicPermission`, `RuntimePermission`, `PropertyPermission`,
`LoggingPermission` — for both `(String)` and `(String,String)`:

```rust
let permission_init = |ctx, args| {
    // writes `name`, returns
};
```

So `BasicPermission.<init>`'s `init(name)` and `PropertyPermission.<init>`'s
`init(getMask(actions))` never ran, and every field those two set — `path`,
`wildcard`, `exitVM`, `mask`, `actions` — stayed at its default.

**Nothing was broken except the constructors.** Invoked reflectively, the real
methods answer exactly as HotSpot does:

```
getMask("read,write")            = 3          (HotSpot: 3)
getMask("bogus")                 throws IAE   (HotSpot: throws IAE)
init(1) then read mask           = 1          (HotSpot: 1)
BasicPermission.init("k") → path = "k"        (HotSpot: "k")
```

That is what makes the diagnosis: the parser works, the initialisers work, and
the objects are still wrong — so it is the callers of those initialisers that
are not running.

### Why it was correct when it was written

`b448f2039` (2026-07-09, WildFly process-controller bootstrap) FABRICATED these
classes. `synthetic_stub_fields` gave `java/security/Permission` a single field
and `synthetic_stub_methods` gave it and its subclasses these very constructors.
On that image, writing `name` **was** the whole implementation.

The real classes are loaded now. Their shape is identical to HotSpot's, method
for method and field for field — `getDeclaredMethods` on
`java.util.PropertyPermission` returns the same eleven names on both VMs. The
closure went on shadowing them anyway.

## Fix

Not by re-deriving the JDK's action parsing in Rust. This codebase already has
the mechanism for a stale synthetic stub: tag the natives
`NativeKind::SyntheticStub` and allow-list the classes in
`real_protected_stub_class`, so `synthetic_stub_should_yield_to_real_bytecode`
hands each method back to the real body whenever the real body is loaded — the
same route `ThreadPoolExecutor.execute` and `java/util/Objects` already take.

On a synthetic image the predicate answers false ("loaded class is itself a
synthetic stub") and these keep serving, which is the configuration they were
written for. The `()V` triple needs no special case: the real JDK declares no
no-arg permission constructor, so the predicate finds no method and leaves it
alone.

Confirmed at the registry level — `--dump-native-registry`, permission-family
rows:

```
before   bridge: 16,  synthetic-stub: 4
after    synthetic-stub: 20
```

## The vector asserts behaviour, not the absence of an exception

The broken VM constructed these permissions happily. It built objects that were
quietly inert, and for a permission class the wrong answer is the dangerous
direction — an `implies()` that returns false is a denial, an `implies()` that
NPEs is at least loud. So `RPermissionInit` asserts the canonical actions
STRING including order and case folding, the `implies` matrix in both
directions, the refusals the JDK owes on malformed input, and every
serialization round trip that first exposed this.

## The reusable shape, and what is still open

**A synthetic-stub carve-out outlives the fabrication it was written for.** The
natives were right when the class was fabricated and became a silent mutilation
the day the real class started loading — with nothing to notice, because the
constructor still returned normally and the object still had a name.

That question generalises, and this page does NOT answer it. A registry census
of the fixed binary:

```
JDK non-Throwable <init> natives:  340
  of which NOT synthetic-stub:     271   across 124 classes
```

Most of those are legitimate — `java/io/FileInputStream`, `java/lang/Thread`,
`java/lang/ClassLoader` and their like need a native constructor because this VM
implements them. Which of the rest are stale carve-outs like this one is a
per-class judgement, not a mechanical one, and it has not been made.
`--dump-native-registry` carries `kind` and `registered_by` (file:line) for
every row, which is the tool for whoever takes it on.

## Repro (pre-fix)

```java
PropertyPermission p = new PropertyPermission("a.b.*", "read,write");
p.getActions();                                    // "" instead of "read,write"
p.implies(new PropertyPermission("a.b.c","read")); // NPE on a null path
new PropertyPermission("k", "bogus");              // accepted; the JDK throws
```
