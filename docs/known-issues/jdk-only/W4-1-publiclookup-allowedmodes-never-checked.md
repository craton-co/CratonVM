# `publicLookup()` reached a private method: `allowedModes` was never written, and never read

**Status:** FIXED in source 2026-08-07 (lane W4-1, JDK-only wave 4). Not
verified against a binary — see *How to verify*. **No out-of-file patch is
required**; two optional hardening patches are listed at the end.

This is the failure lane W3-1 left behind. With
`CRATONVM_MH_STRICT_INVOKEEXACT=1` the run now clears `adaptation()` and dies
in `accessChecks()`.

## The failure

```
CK RJdkHandles invoke ok type=(int,int)int
CK RJdkHandles adapt=14
AssertionError: publicLookup must not reach a private method
```

Identical in `--real-jdk` and `--jdk-only`. HotSpot 25 runs the class to
`PASS RJdkHandles (51 checks)`.

`regression-suite/src/RJdkHandles.java` lines 181-190:

```java
MethodHandles.Lookup pub = MethodHandles.publicLookup();
boolean threw = false;
try {
    pub.findVirtual(Holder.class, "secret", MethodType.methodType(int.class, int.class));
} catch (IllegalAccessException expected) {
    threw = true;
}
check(threw, "publicLookup must not reach a private method");
```

`Holder.secret` is `private`; `Holder` is a nested class of `RJdkHandles`.

## The rule, measured — not remembered

`publicLookup()` is **not** `PUBLIC`. Measured on the JDK in this environment
(`C:\Program Files\Microsoft\jdk-25.0.3.9-hotspot`):

```
PUBLIC=1 PRIVATE=2 PROTECTED=4 PACKAGE=8 MODULE=16 UNCONDITIONAL=32 ORIGINAL=64
MethodHandles.lookup().lookupModes()                      = 95   (0x5F)
MethodHandles.publicLookup().lookupModes()                = 32   (0x20)
MethodHandles.publicLookup().lookupClass()                = class java.lang.Object
MethodHandles.lookup().dropLookupMode(PRIVATE)            = 25   (0x19)
MethodHandles.lookup().in(String.class).lookupModes()     = 1    (0x01)
```

`publicLookup()` carries `UNCONDITIONAL` and nothing else — so a check written
as `modes & PUBLIC` would have been wrong in the permissive direction, and a
check written as `modes == PUBLIC` wrong in the strict one.

## Root cause — two independent halves

### Half 1: `allowedModes` written by slot index onto the real JDK layout

`javap -p java.lang.invoke.MethodHandles$Lookup` gives the instance fields, in
declaration order:

| slot | real JDK 25 | synthetic |
|---|---|---|
| 0 | `lookupClass` (ref) | `lookupClass` (ref) |
| 1 | `prevLookupClass` (ref) | `allowedModes` (int) |
| 2 | `allowedModes` (int) | `previousLookupClass` (ref) |
| 3 | `cachedProtectionDomain` (ref) | `lookupMode` (int) |

`native-builtins/src/lookup_define.rs::alloc_lookup_for` wrote slots 1/2/3 by
index. On a real JDK image that put `Int(0x5F)` into the `prevLookupClass`
*reference* slot, a null ref into `allowedModes` — which then reads back as
**0**, the JDK's "no access at all" — and another `Int(0x5F)` into the
`cachedProtectionDomain` reference slot. Silent: the only symptom is a Lookup
that reports zero modes.

This is the **fifth** instance of the recurring species *slot-index field
access against a real JDK layout* (siblings fixed in wave 3:
`read_module_name`, `parse_nestmate_option`, `register_re7_datagram_socket`,
`Security.getAlgorithms`).

### Half 2 — the one that actually caused the assertion: the access check was dead code

`native-builtins/src/classloader.rs` has a fully written access check
(`enforce_lookup_access`, plus `lk_find_virtual` / `lk_find_static` /
`lk_find_constructor` / `lk_find_getter` / `lk_find_setter` /
`lk_find_static_getter` / `lk_find_static_setter` / `lk_find_special` /
`lk_find_var_handle`, all calling it) and four unit tests that exercise it
directly — including one named
`lk_find_virtual_private_method_with_public_lookup_throws`, which passes.

**None of those functions is ever registered.** `register_classloader_natives`
says so in a comment at the registration site:

```
// findVirtual/findStatic/findConstructor/findGetter/findSetter/findSpecial/
// findVarHandle/findStaticVarHandle are all registered in
// lang_invoke::register_p63_method_handles_lookup — do NOT re-register here
// as that would overwrite the real implementations with incompatible stubs.
```

The live `find*` natives are `lang_invoke.rs`'s `lookup_find_*`, and not one of
them consulted `allowedModes`. So the shipped VM had a green unit-test suite
for an access check the VM never runs — the *"a suite that only asserts the
positive cannot see an indiscriminate impl"* shape, one level worse: the suite
asserts the negative too, of a function nothing calls.

`classloader.rs::lk_public_lookup` is dead for the same reason: it is
registered on `MethodHandles$Lookup.publicLookup()`, but `publicLookup()` is a
static on `MethodHandles`. `lang_invoke.rs`'s registration is the live one.

## The fix

### `native-builtins/src/lookup_define.rs::alloc_lookup_for`

Writes `lookupClass` / `prevLookupClass` / `allowedModes` **by name**, keeping
the index writes as the synthetic-only fallback. Layout discriminator is the
wave-3 one — ~~an absent field answers `Int(0)` from `get_field_by_name`, so a
`Value::Object` answer for `prevLookupClass` means the real class~~:

> **CORRECTED 2026-08-07 — the stated reason is false, and the discriminator as
> written does not do what this paragraph says.** Production
> `get_field_by_name` (`vm/src/vm/vm_exec.rs`) answers **`Value::Object(None)`**
> for an absent field, not `Int(0)`; the `Int(0)`-for-absent convention belongs
> to `MockNativeContext` in `native-builtins/src/test_utils.rs` and to nothing
> else (`native-api/src/test_mock.rs` answers `Object(None)` like production).
> So `matches!(…, Value::Object(_))` matches the absent case as well as the real
> null, and **takes the real-layout arm on the synthetic layout too** — the
> branch it was written to exclude.
>
> The code is nonetheless correct today, for a reason this section predates:
> W6-3 added a positive **class-side witness** as a disjunct in front of the
> value-shape test —
> `ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(obj), "prevLookupClass").is_some()`
> — and the synthetic arm is now reached only when the class genuinely does not
> declare the field. Do not "simplify" the disjunct away on the strength of the
> struck-through sentence. See
> [§4 of *Natives over real JDK classes*](../../architecture/natives-over-real-jdk-classes.md)
> and `docs/feature-designs/by-name-field-reads.md` §1 for where the real
> `Int(0)` comes from (a present-but-**unwritten** reference slot).

```rust
if matches!(ctx.get_field_by_name(obj, "prevLookupClass"), Value::Object(_)) {
    ctx.set_field_by_name(obj, "prevLookupClass", Value::Object(None));
    ctx.set_field_by_name(obj, "allowedModes", Value::Int(LK_FULL_POWER));
} else {
    ctx.set_field(obj, 1, Value::Int(LK_FULL_POWER));
    ctx.set_field(obj, 2, Value::Object(None));
    ctx.set_field(obj, 3, Value::Int(LK_FULL_POWER));
}
```

Slot 0 is `lookupClass` in both layouts and is written both ways, as
`lang_invoke.rs::lookup()` already did.

### `native-builtins/src/lang_invoke.rs::lk_enforce_find_access`

New, and called as the **first statement** of `lookup_find_virtual`,
`lookup_find_static`, `lookup_find_constructor`, `lookup_find_special`,
`lookup_find_getter`, `lookup_find_setter`, `lookup_find_static_getter`,
`lookup_find_static_setter`, `lookup_find_var_handle`,
`lookup_find_static_var_handle`. First-statement placement is deliberate:
`args` still holds the ObjectRefs the VM handed over and nothing has had a
chance to allocate and move them (`lookup_find_special` in particular pins and
calls `ensure_class_initialized` further down, and never pinned `args[0]`).

The rule is **mode bits only**:

* `allowedModes == 0` → allow. A Lookup nobody populated, or one whose modes
  we cannot read. This is the pre-fix behaviour, kept as the valve.
* `allowedModes & PRIVATE != 0` → allow, short-circuiting before the
  allocating `declared_methods` walk. Covers `MethodHandles.lookup()` (0x5F),
  `privateLookupIn` (0x1F) and the JDK's `TRUSTED` lookup (-1).
* `public` member → allow.
* otherwise the member's own modifier picks the required bit: `private` needs
  `PRIVATE`; `protected` needs `PRIVATE|PROTECTED|PACKAGE`; package-private
  needs `PRIVATE|PACKAGE`.
* member flags unresolvable → allow (synthetic stubs, native-registry-only
  classes).

Refusal is `RuntimeError::IllegalAccessException`, which maps to
`java/lang/IllegalAccessException` — a **checked** exception, which is what
`Lookup.find*` declares and what the test catches.

The check deliberately does **not** consult `lookupClass`. Ours comes from a
stack walk in the `MethodHandles.lookup()` native; a wrong answer there must
never become a refusal. This is also why the nestmate case in the same test
(`RJdkHandles`'s lookup reaching `Holder.secret`, line 73-75) keeps working —
that lookup holds `PRIVATE`, and nothing else is asked.

### `native-builtins/src/lang_invoke.rs` — `Lookup.in`

Was a hardcoded `PUBLIC|UNCONDITIONAL` (0x21) — a combination the JDK treats as
impossible, and one that strips `PRIVATE` even for a same-package `in()`, which
the new check would then have turned into a spurious `IllegalAccessException`.
Now: `(prev & FULL_POWER_MODES)` preserved when the two classes share a
package, `PUBLIC` alone otherwise, and never a bare 0 (a 0 would inherit the
"modes unknown, stay permissive" valve). `lk_same_package` answers `true` when
either mirror cannot be named — cannot tell, so do not narrow.

### `native-builtins/src/lang_invoke.rs::lookup_require_field`

The *next* assertion in the same method
(`findGetter(Holder.class, "noSuchField", …)` must raise
`NoSuchFieldException`) would have failed immediately after this fix:
`lookup_find_getter` was deliberately permissive because `resolve_field_index`
answers `None` for synthetic stubs too, so a `None` was not evidence of
absence. The new helper adds the missing evidence — it only refuses when it
could actually enumerate declared fields somewhere on the superclass chain and
the name was not among them. A chain that enumerates empty is still admitted.

## Breadth argument

Everything that legitimately reaches a private member does so through a Lookup
with the `PRIVATE` bit, and the second clause admits those before any member
lookup happens:

| factory | modes | can reach non-public? |
|---|---|---|
| `MethodHandles.lookup()` | 0x5F | yes (PRIVATE) |
| `privateLookupIn` | 0x1F | yes (PRIVATE) |
| `Lookup.defineHiddenClass` (`alloc_lookup_for`) | 0x5F after this fix | yes (PRIVATE) |
| `publicLookup()` | 0x20 | no — the intended refusal |
| `dropLookupMode(PRIVATE)` | 0x5D | no — the intended refusal |
| `Lookup.in`, cross-package | 0x01 | no |
| anything unpopulated | 0 | yes (valve) |

So Spring, Hibernate, Jackson, Groovy, ByteBuddy, CGLIB and the JDK's own
`LambdaMetafactory` — all of which use `MethodHandles.lookup()` or
`privateLookupIn` — are on the PRIVATE fast path and cannot be refused by this
check at all. The only new refusals in the whole VM come from `publicLookup()`,
from an explicitly dropped mode, and from a cross-package `Lookup.in` — the
three cases where HotSpot also refuses.

## Residual (deliberately not enforced)

Nestmate relationships, `protected`-receiver rules, module `exports`/`opens`,
and the `UNCONDITIONAL` "public member of a public *exported* type" rule. All
one-directional: they can only admit something HotSpot refuses, never refuse
something HotSpot admits. `publicLookup().findVirtual(Holder.class, "pub", …)`
is admitted here and refused by HotSpot (`Holder` is package-private) — nothing
in the corpus asks.

~~`Lookup.unreflect` / `unreflectSpecial` (`classloader.rs::lk_unreflect`, the
live registrations) are **not** checked. That is closer to correct than it
looks: the JDK admits `unreflect` of any `Method` whose `setAccessible(true)`
flag is set, regardless of lookup modes. The residual is the
non-`setAccessible` case.~~

> **CLOSED 2026-08-11, and the struck paragraph is wrong twice.**
>
> **Wrong about which registration is live.** `classloader.rs::lk_unreflect` is
> reachable only through `register_classloader_natives`, which reaches the
> registry only through `register_synthetic_overrides` — and
> `vm/src/native/builtins.rs` compiles that to a no-op without the
> `synthetic-jdk` feature. In `--real-jdk` and `--jdk-only` the live natives are
> `lang_invoke.rs`'s `lookup_unreflect` &co., registered by
> `register_t28_method_handle_completeness` straight from `vm_init`. Under
> `--synthetic-jdk` both registrars run and `classloader.rs` is last, so *there*
> its two win — but the other four (`unreflectGetter`, `unreflectSetter`,
> `unreflectVarHandle`, `unreflectConstructor`) have no `classloader.rs` copy
> and route to `lang_invoke.rs` in every mode.
>
> **Wrong about the residual being small.** The `setAccessible` observation
> itself is right — JDK 25's body is `Lookup lookup = m.isAccessible() ?
> IMPL_LOOKUP : this;` — but the non-`setAccessible` case is not a corner. It is
> the whole check this page installed, reachable around in one line:
> `publicLookup().findVirtual(Holder.class, "secret", …)` is refused,
> `publicLookup().unreflect(Holder.class.getDeclaredMethod("secret", int.class))`
> was admitted. Same Lookup, same member, opposite answers.
>
> Fixed in `lang_invoke.rs::lk_enforce_unreflect_access`, sharing
> `lk_modes_required_for_member` with `lk_enforce_find_access` so the two gates
> cannot drift apart again. Full rule, the quoted JDK 25 javadoc for each of the
> three *different* `accessible`-flag treatments, and the mode-scope argument:
> W6-8, section "The `Lookup.unreflect*` row".

## Optional hardening patches — BOTH ALREADY IN THE TREE (checked 2026-08-11)

**Do not apply these. They are done.** Verified against
`native-builtins/src/classloader.rs` before writing a line, per this campaign's
finding that records claim a hand-off patch was never applied when it is in fact
in the tree.

1. `lk_drop_lookup_mode` — already reads `let modes = lk_modes_of(ctx, this);`,
   carrying the comment "See `lk_in_method` for why this must not read
   `LK_ALLOWED_MODES` raw." It went further than this page asked: a receiver
   reporting 0 now stays 0 instead of falling through to `LK_FULL_POWER`,
   because dropping a mode must never *grant* one.
2. `lk_in_method` — same, `let modes = lk_modes_of(ctx, this);`, and likewise
   keeps a 0 at 0 ("`in()` never GRANTS access the receiver did not have",
   measured). It also gained `lk_check_in_target` (the primitive / array / null
   argument shapes) and `lk_class_relation`, which answers the same-package
   question *plus* the two a same-package test cannot: is the target the lookup
   class itself, and are the two members of the same top-level class.
   `lang_invoke.rs::lk_same_package` is correspondingly caller-free and
   documented as superseded — a same-package test alone over-grants
   `PRIVATE|PROTECTED`, measured 31 where OpenJDK 25.0.3 answers 25.

The dead `enforce_lookup_access` / `lk_find_*` block in `classloader.rs` and its
four unit tests are **still there**. `classloader.rs` is not this lane's file:

### Out-of-file patch (not applied)

No behavioural patch is outstanding — both hardening items are already in. The
one remaining action is deletion-only and carries no behaviour change: remove
`enforce_lookup_access`, `lk_find_virtual`, `lk_find_static`,
`lk_find_constructor`, `lk_find_getter`, `lk_find_setter`,
`lk_find_static_getter`, `lk_find_static_setter`, `lk_find_special`,
`lk_find_var_handle` and `lk_public_lookup` from
`native-builtins/src/classloader.rs`. Every one is unregistered — the
registration site says so in its own comment — and `dead_code` is allowed
crate-wide (`native-builtins/src/lib.rs:9`), so nothing warns that they are
unreachable. **Re-point the four unit tests rather than delete them**: aim
`lk_find_virtual_private_method_with_public_lookup_throws` and its siblings at
`lang_invoke.rs::lk_enforce_find_access`, which is the function that actually
runs. A green suite for a check the VM never invokes is what let this defect
ship.

## How to verify

```
CRATONVM_MH_STRICT_INVOKEEXACT=1 target/release/cratonvm \
  --java-home "$JDK25" --jdk-only -cp regression-suite/classes RJdkHandles
CRATONVM_MH_STRICT_INVOKEEXACT=1 target/release/cratonvm \
  --java-home "$JDK25" --real-jdk -cp regression-suite/classes RJdkHandles
```

Expected next line after `CK RJdkHandles adapt=14` is
`CK RJdkHandles accessChecks ok`, then `CK RJdkHandles varhandle …` and
`PASS RJdkHandles (51 checks)`. The remaining 21 checks are `varHandles()`,
which this lane did not touch and has never been reached.

**Single falsifying observation:** the run reaches `accessChecks` and dies on
`private nestmate access` (line 75) or `findVirtual invokeExact` (line 70)
instead — that would mean `MethodHandles.lookup()`'s `allowedModes` is not
landing as 0x5F on the real layout, and the check is refusing a full-power
lookup. Confirm with `CRATONVM_DBG_LOOKUP=1`, which prints
`[DBG_LOOKUP] lookup(): byname_allowedModes=…`.
