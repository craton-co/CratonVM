> **FIXED 2026-08-11 — moved out of `docs/known-issues/jdk-only/`.**
>
> Vector `RJdkHidden` passes in the 53/1 run. Entirely in-lane. "Residual on this vector" was an expectation about the checks behind `:151` ("Expected green"), which the vector passing settles; "Optional companion patch (shadowed today)" concerns `classloader.rs::lk_define_hidden_class`, which this record itself establishes is dead code on every live path because a later registration overwrites the descriptor.
>
> Previous location: `docs/known-issues/jdk-only/W3-2-non-nestmate-hidden-class-nest-host.md`.
> Audit that moved it: `docs/known-issues/jdk-only/RETIREMENT-20260811.md`.

# A non-nestmate hidden class joined its template's nest, because the NESTMATE option was never decoded

Status: fix written, unbuilt (lane W3-2 cannot run cargo or the VM).
Applies to: **both** `--real-jdk` (Compatible) and `--jdk-only` (JdkOnly),
identically. HotSpot 25 passes all 30 checks with exit 0, so this is an
ordinary Compatible-mode defect, not a strict-mode policy question.

## The failure

`regression-suite/src/RJdkHidden.java`, both arms, byte-identical output up to
the throw:

```
CK RJdkHidden nestMembers=3
CK RJdkHidden hidden=true nestHostIsLookup=true namePrefix=true call=priv:x/4242 static=8484
Exception in thread "main" java/lang/AssertionError: a non-nestmate hidden class is its own nest host
    at RJdkHidden.main(RJdkHidden.java:175)
    at RJdkHidden.defineNonNestmate(RJdkHidden.java:151)
```

```java
// RJdkHidden.java:147-151
static void defineNonNestmate() throws Throwable {
    byte[] bytes = payloadBytes();
    Class<?> hc = MethodHandles.lookup().defineHiddenClass(bytes, false).lookupClass();
    check(hc.isHidden(), "non-nestmate hidden class");
    check(hc.getNestHost() == hc, "a non-nestmate hidden class is its own nest host");
```

## The oracle

Measured on HotSpot 25 (`jdk-25.0.3.9-hotspot`), not asserted from the spec —
`.hs` reference output:

```
CK RJdkHidden nonNestmateOwnHost=true
CK RJdkHidden checks=30
PASS RJdkHidden (30 checks)
```

`Lookup.defineHiddenClass(bytes, initialize)` **without**
`ClassOption.NESTMATE` puts the hidden class in a nest of its own:
`hc.getNestHost() == hc`. With `NESTMATE` it joins the lookup class's nest.

## Two defects, and they were cancelling each other out

This reads as one assertion but it is two independent bugs whose errors
happened to sum to zero on the arm that passed.

### 1. `nest_host_class_name = None` fell back to the class file's `NestHost`

`classloading/src/class_manager.rs::define_class_with_options` did

```rust
let nest_host = options.nest_host_class_name.clone().or(nest_host);
```

so an unset option meant "use whatever the bytes declare". The bytes here are
**javac's own** `RJdkHidden$Payload.class`, read back off the class path as a
resource and defined a second time — and javac emits a `NestHost` attribute for
every nested class (`javap -v` confirms `NestHost: class RJdkHidden`). The
hidden class therefore adopted `RJdkHidden` as its nest host and
`Class.getNestHost()` answered the outer class.

HotSpot reaches "itself" by a route this VM cannot reproduce verbatim:
`InstanceKlass::nest_host()` resolves the `NestHost` attribute and then requires
the resolved host to list the claimant in its own `NestMembers`. A hidden class
is registered under a mangled, per-definition name (`RJdkHidden$Payload/0x1f`),
and `RJdkHidden`'s `NestMembers` is a compile-time attribute listing exactly
`RJdkHidden$Payload` and `RJdkHidden$Callable` — it can never spell the mangled
name. The round-trip fails **unconditionally, by construction**, and HotSpot
silently falls back to self-nest.

Because the failure is unconditional, evaluating the predicate once at
definition is equivalent to evaluating it on every query. That is what
`hidden_class_drops_class_file_nest_host` now does. It is the mirror image of
the exemption `classloading/src/access_control.rs::confirmed_nest_host` already
records: *there*, a hidden class's supplied host is trusted precisely because
the `NestMembers` round-trip is unsatisfiable for a mangled name.

### 2. `parse_nestmate_option` read slot 0, which on a real enum is `name`

`native-builtins/src/lookup_define.rs::parse_nestmate_option` did

```rust
if let Value::Int(ord) = ctx.get_field(opt, 0) { if ord == 0 { return true; } }
```

with the comment "a synthetic ClassOption enum object whose field 0 holds its
ordinal". That is true under `synthetic-jdk`. Under `--real-jdk` the varargs
array carries genuine JDK enum constants, and this native is deliberately
registered for that mode too — `native-builtins/src/reflect_annotations.rs`
wires it precisely so the real `Lookup.defineClass` bytecode does not descend
into `defineClass1` with a 0-length byte view (Gradle's `LookupClassDefiner`).

`javap -p java.lang.Enum` (JDK 25) declares `name` first:

```
private final java.lang.String name;    // slot 0
private final int ordinal;              // slot 1
private int hash;                       // slot 2
private final int flag;                 // ClassOption's own, slot 3
```

So slot 0 is a `String` reference, the `Value::Int` arm never matched, and
`nestmate` was `false` for **every** call — NESTMATE included.

### Why the NESTMATE arm still passed

Defect 2 left `nest_host_class_name` unset on the NESTMATE arm; defect 1 then
filled it from the class file's `NestHost`, which for these bytes is
`RJdkHidden` — exactly the answer NESTMATE wanted. `RJdkHidden.java:101` passed
for the wrong reason.

**Fixing defect 1 alone moves the failure backwards onto `:101`.** Both are
required, and neither is visible from the arm that was green.

## The fix

* `class_manager.rs`: a new `hidden_class_drops_class_file_nest_host(hidden,
  explicit)` gates the fallback. `hidden && explicit.is_none()` → self-nest.
  Non-hidden classes are untouched and still go through the full bidirectional
  `NestMembers` confirmation; an explicit host (the NESTMATE arm, and
  `Unsafe.defineAnonymousClass`'s host) stays authoritative because only the
  defining call could have made that claim.
* `lookup_define.rs`: `class_option_is_nestmate` decides on positive evidence
  only, in three steps — the enum constant's `name` (`"NESTMATE"` / `"STRONG"`),
  then `ClassOption.flag` against `NESTMATE_CLASS = 0x1`, then the synthetic
  slot-0 ordinal. An unrecognised shape falls through to the historical
  behaviour rather than guessing. The flag step requires a **non-zero** value
  ~~because a by-name read of an absent field answers `Int(0)`~~; both real
  constants have a non-zero flag (`NESTMATE = 0x1`, `STRONG = 0x4`).

  > **CORRECTED 2026-08-07 — the stated reason is false; the guard is still
  > right, for a different reason.** Production `get_field_by_name`
  > (`vm/src/vm/vm_exec.rs`) answers `Value::Object(None)` for an **absent**
  > field, so an `Int` match never sees the absent case at all and the non-zero
  > test does nothing about it. What the non-zero test does guard is the
  > *present-but-**unwritten*** slot, which really does decode as `Int(0)` —
  > `docs/feature-designs/by-name-field-reads.md` §1. Absent-vs-present is
  > answered by asking the class
  > (`resolve_field_index_by_class_id(class_id_of_object(o), name)`), never by
  > the value. See
  > [§4 of *Natives over real JDK classes*](../../architecture/natives-over-real-jdk-classes.md).

`ClassOrigin` classification deliberately keeps reading the *declared* nest
host, so no origin verdict changes: `GeneratedLambda { host }` is a provenance
label, not a nest-membership decision.

All four producers of `hidden: true` agree on the discriminator — they set
`nest_host_class_name` if and only if NESTMATE was requested — so the
`class_manager` gate covers every route:
`lookup_define.rs` (both variants), `classloader.rs::lk_define_hidden_class`,
`lang_system.rs::native_classloader_define_class0`.

## Reproduce

```
cd regression-suite && <cratonvm> --java-home "<jdk>" [--jdk-only] -cp build RJdkHidden
```

## Residual on this vector

`defineNonNestmate` has never executed past `:151` on this VM. Behind it:

* `:155` `Callable.class.isAssignableFrom(hc)` — the same predicate as `:98`,
  which already passes on the same bytes through the same
  `resolve_lookup_supertypes` path. Expected green.
* `:156` prints and returns; `main` then prints `checks=30`.

The wave-2 wall (`Lookup.findConstructor` / `findStatic` resolving hidden
classes by name) is **not** on this path — `defineNestmate` already reaches
`:116-:127` and the `CK RJdkHidden hidden=... call=priv:x/4242 static=8484`
line proves both handles invoked successfully.

## Optional companion patch (shadowed today)

`native-builtins/src/classloader.rs::lk_define_hidden_class` (~line 8238)
carries the same slot-0 ordinal read. It is registered *before*
`register_lookup_define_class` and therefore overwritten for this descriptor,
so it is dead code on every live path. Worth converging on
`class_option_is_nestmate` if that registration order ever changes.
