# `publicLookup()` reached a private method: `allowedModes` was never written, and never read

**Status (reconciled 2026-08-12 — W7-55-record-reconciliation.md):**

* **Headline: CLOSED in source** (2026-08-07, commit `256d119b4`). Half 1 —
  `alloc_lookup_for` writes by name with a class-side witness,
  `native-builtins/src/lookup_define.rs:268`, witness at `:335`/`:364`. Half 2 —
  `lk_enforce_find_access` is the first statement of the ten `lookup_find_*`,
  `native-builtins/src/lang_invoke.rs:4232`; `Lookup.in` mode computation at
  `:4584`, `lookup_require_field` at `:5228`.
* **Residual: CLOSED — all four the 2026-08-11 block names, plus the two
  "optional hardening patches".** `unreflect` is gated —
  `lk_enforce_unreflect_access` (`lang_invoke.rs:4452`) called first at all six
  sites (`:11033`, `:11079`, `:11149`, `:11193`, `:11234`, `:11389`), commit
  `3644142d5`. `UNCONDITIONAL`'s class half — `fd86485c6`,
  `lang_invoke.rs:4283-4292`. A zero-mode `Lookup` is refused and `Some(0)` is
  separated from "unreadable" — `6dd552ce2`, `lk_read_allowed_modes_opt` at
  `lang_invoke.rs:4110`, refusal at `:4258`. `Lookup.in` rejects primitive/array
  targets — `680699467`. The two hardening patches landed on **2026-08-07** in
  commit `dcfe77cb8`: `lk_drop_lookup_mode` at
  `native-builtins/src/classloader.rs:9571` (carrying the exact "See `lk_in_method`
  for why this must not read `LK_ALLOWED_MODES` raw" comment) and `lk_in_method`
  at `:9543`.
  **RETIREMENT-20260811.md is WRONG on this record** — its kept-list reason,
  *"two optional hardening patches … that are still unapplied"*, was already
  false four days before that audit ran. This page's own 2026-08-11 block is the
  correct one.
* **Residual: FULLY CLOSED 2026-08-12 — but W7-62 left two of the same species
  behind, and they are closed here.** W7-62 deleted the dead block and moved the
  six tests aimed at `lk_find_*`. It did **not** look at
  `test_lk_lookup_registered` (`native-builtins/src/classloader.rs`) or
  `test_lk_public_lookup_registered`, which assert that
  `MethodHandles$Lookup.lookup()` and `MethodHandles$Lookup.publicLookup()` are
  registered. **No real JDK declares either method on `Lookup`** — `lookup()`,
  `publicLookup()` and `privateLookupIn(..)` are all `static` members of
  `java.lang.invoke.MethodHandles`, and this module's own doc comment on
  `lk_public_lookup` says so in place: *"This registration sits on
  `MethodHandles$Lookup`; the live `MethodHandles.publicLookup()` static is
  `lang_invoke.rs`'s."* So both tests were green over triples no live path can
  dispatch to — the identical shape as the six W7-62 moved, on the very entry
  point this record is named for.
  **W7-62's reason for keeping `lk_public_lookup` was right about the fact and
  wrong about what it licensed: registered is not reachable.** The function
  stays (deleting a registration on the strength of a source read is how this
  campaign has made things worse), but both tests now assert the LIVE
  `java/lang/invoke/MethodHandles` triple FIRST — the assertion that goes red if
  `lang_invoke::register_p63_method_handles_lookup` ever stops registering the
  static — and keep the `LK_CLASS` twin as a second, explicitly labelled
  `synthetic-jdk`-only row. The older text follows:
* **Residual: WAS OPEN — one, and it is a deletion, not a behaviour change.**
  The `## Out-of-file patch (not applied)` below is genuinely unapplied. The dead
  block in `native-builtins/src/classloader.rs` survives: `lk_public_lookup`
  (`:8436`), `enforce_lookup_access` (`:9151`), `lk_find_virtual` (`:9216`),
  `lk_find_static` (`:9236`) and siblings — and, worse, **four unit tests still
  aim at the dead function** (`classloader.rs:13004`, `:13028`
  `lk_find_virtual_private_method_with_public_lookup_throws`, `:13110`), so they
  assert nothing about the code that actually runs. Zero behaviour change; a
  cleanliness item and a trap. Re-grepped 2026-08-12.
* **Deliberate, not pending** — three refusals with reasons in source:
  `UNCONDITIONAL`'s "unconditionally exported package" half is one-directional
  because there is no module graph to ask (`lang_invoke.rs:4277-4281`); and
  nestmate / `protected`-receiver / module-`exports` checks are outside
  `lk_enforce_find_access` by design.
* **Two of this page's own claims are wrong and are struck below — do not
  "simplify" on them.** The layout-discriminator rationale (that
  `get_field_by_name` returns `Int(0)` for an absent field) is false for the
  production path; the code is correct only because of W6-3's class-side witness
  at `lookup_define.rs:335`. And "`allowedModes == 0` → allow" is superseded by
  `6dd552ce2`.
* **Cannot adjudicate without a run:** `CRATONVM_MH_STRICT_INVOKEEXACT=1
  target/release/cratonvm --java-home "$JDK25" --jdk-only -cp
  regression-suite/classes RJdkHandles`, and the same with `--real-jdk`;
  expect `PASS RJdkHandles (51 checks)`. Note
  W7-11-strict-baseline-remeasured.md reports the strict suite at 68/0, which
  covers `RJdkHandles`.

> **2026-08-11 — the residuals are closed, and two of this page's claims were
> wrong.** Nothing below is a work item.
>
> * The **two "optional hardening patches"** at the end are **already in the
>   tree**, and went further than this page asked. Do not re-apply them.
> * The **`unreflect` residual** is fixed
>   (`lang_invoke.rs::lk_enforce_unreflect_access`). This page called it small;
>   it was the whole check, reachable around in one line.
> * This page names `classloader.rs::lk_unreflect` as "the live registrations".
>   **It is not**, in `--real-jdk` or `--jdk-only`. Corrected in place.
> * The `lk_enforce_find_access` described under *The fix* has since gained two
>   arms this page predates: a genuine zero-mode Lookup is now refused (`Some(0)`
>   and "could not read" are kept apart), and `UNCONDITIONAL` now requires the
>   target CLASS to be public. The "Residual" section is updated accordingly.

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

> **This paragraph is correct and outlived the patch block that acted on it.**
> W7-62's deletion list rejected the `lk_public_lookup` row on the grounds that
> it *is* registered — true, and beside the point, because the triple it is
> registered on names no method any real JDK declares. The function is kept; the
> two unit tests that read as coverage of it were re-pointed on 2026-08-12 (see
> the status block). The same is true of `lk_lookup` and `lk_private_lookup_in`,
> which this paragraph does not name and which sit on `LK_CLASS` for the same
> reason.

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

* ~~`allowedModes == 0` → allow. A Lookup nobody populated, or one whose modes
  we cannot read. This is the pre-fix behaviour, kept as the valve.~~
  **STRUCK — SUPERSEDED, DO NOT RE-IMPLEMENT. Reconciled 2026-08-12.** This is
  the second of the two claims the status block says are struck below; it was
  the one that had not actually been struck. A genuine zero-mode `Lookup` is now
  REFUSED every member, public ones included, since commit `6dd552ce2`:
  `lk_read_allowed_modes_opt` (`native-builtins/src/lang_invoke.rs:4110`) keeps
  `Some(0)` apart from "could not read the field", and the refusal is at `:4258`.
  Only the *unreadable* case still allows. Detail in the *Residual* section
  below and in W7-55-record-reconciliation.md §2.4.
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
~~and the `UNCONDITIONAL` "public member of a public *exported* type" rule~~.
All one-directional: they can only admit something HotSpot refuses, never refuse
something HotSpot admits. ~~`publicLookup().findVirtual(Holder.class, "pub", …)`
is admitted here and refused by HotSpot (`Holder` is package-private) — nothing
in the corpus asks.~~

> **Two of these have since been enforced** (2026-08-11 re-read; both predate
> this lane's work and neither is mine):
>
> * The `UNCONDITIONAL` rule's **class half** is enforced —
>   `publicLookup().findVirtual(Holder.class, "pub", …)` is now refused,
>   measured against OpenJDK 25.0.3 as `IllegalAccessException: symbolic
>   reference class is not accessible`. The struck example is out of date. Only
>   the "unconditionally *exported* package" half remains unenforced, for want
>   of a module graph, and that half stays one-directional.
> * A genuine **zero-mode Lookup** is now refused every member, public ones
>   included (`lookup().dropLookupMode(PUBLIC)` is 0 and refuses a public method
>   of a public class on OpenJDK 25.0.3). That required `Some(0)` and "could not
>   read the field" to stop being the same value — see
>   `lk_read_allowed_modes_opt`. The `allowedModes == 0` → allow bullet under
>   *The fix* describes the pre-2026-08-11 behaviour and is superseded.
>
> Both rules are mirrored into the new `unreflect` gate, which is the point of
> the two gates sharing `lk_modes_required_for_member`.

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

### Out-of-file patch — APPLIED 2026-08-12 (W7-62-ratchets-and-dead-code.md)

**Done, and one line of the prescription below was wrong.** The dead block is
deleted and the tests were re-pointed rather than dropped, as this section asks.
Recorded here rather than left for a re-grep, because that is the rule
W7-55-record-reconciliation.md ends on.

* **Deleted** from `native-builtins/src/classloader.rs`: `enforce_lookup_access`,
  `lk_member_access_flags`, and the eleven `lk_find_*` bodies
  (`lk_find_static_var_handle` included — this section's list omits it).
  332 lines, replaced by a tombstone that names what supersedes them.
* **`lk_public_lookup` was NOT deleted, and must not be.** This section's list
  is wrong about it: it IS registered, on
  `MethodHandles$Lookup.publicLookup()`. Deleting it would have dropped a live
  registration. That is the §2.4 species — an observation that outlived its
  prescription — inside this record's own patch block.
* **The tests were SIX, not four**, and they moved to `lang_invoke.rs`'s test
  module aimed at `lk_enforce_find_access`. Four arms the old ones could not
  express went with them, the first of which is the point of this whole record:
  every one of the old tests used `LK_PUBLIC` (0x01) as the `publicLookup()`
  mode word, and `publicLookup()` is `UNCONDITIONAL` (0x20) and carries no
  `PUBLIC` bit at all — so they asserted about a Lookup shape the JDK never
  hands out. The new rows also cover the `UNCONDITIONAL` class rule, a
  zero-mode Lookup, an unreadable mode word (which must stay permissive), and
  the positive private case this record's own test-module NOTE claimed could
  not be exercised in-unit. That note was describing a reader that had already
  been replaced; it is gone with the tests.

The original prescription follows, for the record.

### Out-of-file patch (as prescribed, superseded by the block above)

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
