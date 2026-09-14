# G12-1 — the proxy that any interface array accepted

**Status:** FIXED IN SOURCE. **The "before" is MEASURED on a CratonVM binary**
(see §1); the "after" is **PREDICTED** — this lane could not build or run (the
orchestrator held the differential suite). **Provenance:** oracle side MEAS
throughout; CratonVM side MEAS before, PRED after.

Oracle: HotSpot 25.0.3+9-LTS (Microsoft build), windows/x64, at `$JAVA_HOME`.
Probe: `scratchpad/g12/ProxyStoreProbe.java`, one execution per shape.
Lane G12, 2026-08-17. Files owned and touched:
`vm/src/runtime/interpreter/typecheck.rs`, `vm/src/jit/helpers.rs`. Everything
else is a NOMINATION in §8.

Closes: **`W7-101` and `W8-C10-1` — see §7, which says on what evidence and
what each of them is still owed.**

---

## 1. The measurement this record starts from

Unlike every record in this directory before `F41-1`, and unlike `W7-101`
itself, the "before" here is not a prediction. The orchestrator built a binary
from the current tree — merge commit `d87dff06a` ("Merge branch 'dev' into
claude/jdk-only-mode-completion-1351c0") plus two orchestrator fixes — and
diffed `RArrayStoreInterfaces` against the oracle. **Exactly one row of 108
diverged:**

```text
s12  Runnable[]  <- Proxy(Marker)
  HotSpot   cold=[ArrayStoreException]  hot=[ArrayStoreException]
  CratonVM  cold=[no-throw]             hot=[no-throw]
```

with `illegalRefused=11/12` (HotSpot `12/12`), `legalAdmitted=15/15`,
`fails=2`, vector RED.

Three things follow from the shape of that row before any source is read, and
each one deletes a hypothesis:

* **`cold` and `hot` are identical.** So this is not a JIT-lowering defect and
  not a merge artefact of the recent `aastore` ABI unification. The *shared*
  refusal decision is wrong at the source. (`W7-38` and `W8-E11-1` are the
  records for the other shape — a compiled tier that never asked — and both
  present as a tier SPLIT, which this is not.)
* **`s25 Marker[] <- Proxy(Marker)` is green and `legalAdmitted=15/15`.** So
  proxies are not being ignored wholesale. The predicate admits a proxy into an
  array of an interface it *does* have.
* **`s16`–`s23` are green.** Ordinary interface components work. The gap is
  specific to how a dynamic proxy's interface set is consulted.

`legalAdmitted=15/15` is worth dwelling on, because it is the number that says
nothing. A predicate that admits every proxy gets every LEGAL proxy row right
*for free*; `s25` and `s26` were green on a rule that never looked at anything.
That is the reason `RArrayStoreInterfaces` pairs each legal store with an
illegal store into the same array and reports the two counts separately, and it
is why the whole defect showed up as **one row out of 108**.

## 2. What the oracle actually does with a proxy — MEASURED

`scratchpad/g12/ProxyStoreProbe.java`, transcribed:

```text
--- proxy class facts ---
pA class               = $Proxy0
pA superclass          = java.lang.reflect.Proxy
pA interfaces          = [interface ProxyStoreProbe$A]
pA instanceof Ser      = true
pAB interfaces         = [interface ProxyStoreProbe$A, interface ProxyStoreProbe$B]
pSub instanceof SuperI = true
Proxy implements       = [interface java.io.Serializable]
Proxy isInterface      = false
pA==pAB class same     = false
--- stores ---
A[]            <- proxy(A)      OK
B[]            <- proxy(A)      java.lang.ArrayStoreException | $Proxy0
Unrelated[]    <- proxy(A)      java.lang.ArrayStoreException | $Proxy0
Runnable[]     <- proxy(A)      java.lang.ArrayStoreException | $Proxy0
A[]            <- proxy(A,B)    OK
B[]            <- proxy(A,B)    OK
Unrelated[]    <- proxy(A,B)    java.lang.ArrayStoreException | $Proxy1
SubI[]         <- proxy(SubI)   OK
SuperI[]       <- proxy(SubI)   OK
SubI[]         <- proxy(SuperI) java.lang.ArrayStoreException | $Proxy3
Object[]       <- proxy(A)      OK
Serializable[] <- proxy(A)      OK
Cloneable[]    <- proxy(A)      java.lang.ArrayStoreException | $Proxy0
Proxy[]        <- proxy(A)      OK
$Proxy0[]      <- proxy(A)      OK
$Proxy1[]      <- proxy(A)      java.lang.ArrayStoreException | $Proxy0
A[]            <- null          OK
A[][]          <- A[]           OK
A[][]          <- proxy(A)      java.lang.ArrayStoreException | $Proxy0
Object[][]     <- A[]           OK
```

**The rule is that there is no rule.** HotSpot has no proxy-specific `aastore`
arm at all. A `$ProxyN` is an ordinary class: its interface set is the argument
list `Proxy.newProxyInstance` was handed, in that order, and its superclass is
`java.lang.reflect.Proxy`. Every row above is JVMS §6.5 applied to that class
with no adjustment.

Two rows are the ones a lane would otherwise assume wrongly, and the task asked
that they be verified rather than assumed:

* **Every proxy IS `Serializable` — and it is not a proxy rule.**
  `java.lang.reflect.Proxy.class.getInterfaces()` is `[java.io.Serializable]`,
  so the proxy inherits it through its superclass like any other subclass would.
* **`Cloneable` is the control that proves the previous sentence.**
  `Cloneable[] <- proxy(A)` THROWS. If "proxies satisfy the marker interfaces"
  were the rule, it would not. The asymmetry is inheritance, not exemption.

And two more that matter for the fix's shape: the walk is **transitive**
(`SuperI[] <- proxy(SubI)` is OK) and **directional** (`SubI[] <- proxy(SuperI)`
throws), and a proxy's *own generated class* is a legal component while a
*different* proxy class is not.

The ASE message names the value's own class (`$Proxy0` — default package here,
so no `jdk.proxyN` prefix), consistent with `W8-E6-1`. The fixture does not
assert `s12`'s message (`coldSkip(12)`; the cold pass `continue`s on it and
never publishes it), because the generated number is not stable — so nothing in
this change can move a message assertion.

## 3. The mechanism — why the predicate answered "allowed"

`vm/src/runtime/interpreter/typecheck.rs`, `aastore_element_assignable`. The
value's class is a real generated `$ProxyN` (`jdk/proxy<M>/$Proxy<N>`), so:

* `array_descriptor_of` → `[Ljava/lang/Runnable;`, component `java/lang/Runnable`;
* the by-name superclass walk declines (`$ProxyN` → `java.lang.reflect.Proxy` →
  `Object`, no `Runnable` on the chain);
* `is_subclass_of(value, comp)` declines (the proxy's `interfaces` vector holds
  `Marker`, not `Runnable`);
* `is_assignable_to_name(value, "java/lang/Runnable")` declines for the same
  reason;
* the synthetic-id range test (`>= 0x8000_0000`) declines — a real generated
  proxy is a real loaded class with a real dense id;
* and then:

```rust
let vn: &str = &*cls.name;
if vn == "java/lang/annotation/AnnotationProxy"
    || vn.ends_with("AnnotationProxy")
    || vn.contains("$Proxy")
{
    return true;
}
```

**`vn.contains("$Proxy")`.** That is the whole mechanism. Not a missing
interface walk, not a fail-open `Err(_) => allow`, not a `class_id` resolving to
the superclass: a **name test standing in for an interface set that the VM
already holds**. `is_subclass_of` and `is_assignable_to_name` had both answered
correctly and precisely; a substring match on the class's own name then threw
their answer away.

### 3.1 And it was not the only arm that would have

`W7-101` §3 states the lesson this record then had to apply: *when two
fail-open guards can produce the same wrong answer, fixing the one you found
does not tell you whether it was the one that fired — source order does.* It is
still true here, and there were still two:

| # | arm | matches a generated `$ProxyN` by | fires |
|---|---|---|---|
| 13 | `vn.contains("$Proxy")` | its own NAME | first |
| 14 | `class_chain_reaches_proxy_instance(value)` | its SUPERCLASS (`java/lang/reflect/Proxy`, the default super since the real-super gate) | immediately after |

Arm 13 is what fired. Arm 14 would have caught the same value on the very next
line, so scoping arm 13 alone would have produced a green build, no failing
test, and an unchanged `s12`. **Both had to move.** Arm 15
(`synthetic_implements`) was checked and declines for a proxy on its own: no arm
of it keys on a `$ProxyN` name, its `AnnotationProxy` arm matches by exact class
name, and `java/lang/Runnable` is not one of its targets.

### 3.2 Why the name test was there, and what changed under it

The comment's justification is the same one `W7-101` §4 dismantled for the
interface blanket, one level down: a dynamic proxy acquires its interfaces at
runtime, invisibly to the static hierarchy, so refusing on hierarchy evidence
alone would throw spuriously.

**That was true when it was written and is now true of a strictly smaller
population.** Since `proxy-real-classfile`, `Proxy.newProxyInstance` does not
produce an interface-less shim: `define_or_get_proxy_class`
(`native-builtins/src/reflect_annotations.rs`) emits real `proxy_gen` bytes and
defines them with `interface_id_overrides` — *the exact `ClassId`s the caller
passed* — and `classify_defined_origin`
(`classloading/src/class_manager.rs`) records them on
`ClassOrigin::GeneratedProxy { interfaces }`. A generated proxy's interface set
is not missing information. It is the proxy's whole identity, stated by the code
that generated the class, in two places at once.

This is the same shape as `W7-101` and `W8-C4-2` and it keeps recurring: **a
fail-open guard written before its precise successor existed does not become
harmless when the successor arrives — it becomes the reason the successor never
runs.** The difference this time is that the successor is not another arm below
it in the same function; it is a field on the class, which is why reading the
function top-to-bottom would not have found it.

## 4. The fix

One discriminator, in `typecheck.rs`:

```rust
fn recorded_proxy_interface_set(origin: &cratonvm_classloading::ClassOrigin) -> Option<&[ClassId]> {
    match origin {
        ClassOrigin::GeneratedProxy { interfaces } if !interfaces.is_empty() => Some(&**interfaces),
        _ => None,
    }
}
```

**The emptiness test is load-bearing, not defensive.** `ensure_synthetic_class`
fabricating a class from a `$ProxyN` NAME also produces
`GeneratedProxy { interfaces: [] }` — pinned in-tree, not inferred, by
`classloading/tests/jdk_only_class_origin.rs`'s
`fabricated_generated_names_are_not_compatibility_stubs`, which asserts exactly
that origin for `com/example/$Proxy42`. An empty list means "no record", not
"implements nothing". Dropping `is_empty` would turn every fabricated proxy name
into a refusal, which is the `AotIntegrationTests` regression in reverse.

With that, three changes in `aastore_element_assignable`:

1. **A positive walk, which can only ADMIT.** For a recorded proxy: allow if the
   component is `java/io/Serializable` or a proxy superclass
   (`class_name_is_proxy_super`, the existing shared answer — both MEASURED
   above), otherwise `is_assignable_to_name(iface_id, comp_name)` for each
   recorded interface. That is the same loader-blind by-name walk the component
   check above uses, so it is transitive through super-interfaces and survives a
   split loader.
2. **Arm 13 scoped** to `!proxy_interface_set_is_recorded`. The `AnnotationProxy`
   terms are untouched in substance — an `AnnotationProxy` carrier is not a
   `GeneratedProxy`, so it never trips the gate.
3. **Arm 14 scoped** the same way.
   `class_chain_reaches_proxy_instance` **itself is unchanged**: it is
   `pub(crate)` and four other callers ask it a different question.

The positive walk in (1) is deliberately redundant with `is_subclass_of` and
`is_assignable_to_name` above it. That redundancy is the safety margin for the
one row that could go the wrong way: it asks the ORIGIN's id list rather than
the `Class`'s `interfaces` vector, so `s25` and `s26` stay admitted on **two
independent roads** even if those two vectors ever drift. It cannot cause a
refusal — falling through it is what does that.

### 4.1 One edit, both arms — confirmed rather than assumed

The task asked that the shared-predicate claim be checked, not taken. It was,
by enumerating the callers:

```text
vm/src/runtime/interpreter/opcodes.rs:287   the interpreter `aastore` opcode
vm/src/jit/helpers.rs:5423                  aastore_store_is_refused, reached by
                                            BOTH jit_aastore and
                                            jit_aastore_type_check (the x64
                                            inline lowering)
native-builtins/src/lib.rs:41851            reflective Array.set, via
                                            ctx.aastore_element_assignable
```

Three callers, one predicate, no fourth. So the `cold`/`hot` pair moves
together — and the reason the divergence was identical in both tiers in the
first place is the same fact. **`vm/src/jit/helpers.rs` needed no behavioural
change**, and that is the finding, not an omission: the only edit there is the
doc comment on `aastore_store_is_refused`, which claimed the predicate "fails
OPEN on imprecise type information" without saying that a recorded proxy's
interface set is not imprecise — plus a test module that pins the linkage, since
this exact file has already been the site of a comment-only premise going
silently false (R20/HIGH-5, `W7-38`).

Reflective `Array.set` inherits the fix, which is correct: HotSpot applies one
rule to `aastore` and `Array.set`, as `native-builtins/src/lib.rs:41819` already
says in terms.

## 5. Why no currently-green row of `RArrayStoreInterfaces` can flip — s1…s27

The change has exactly one entry condition: the VALUE's class origin is
`ClassOrigin::GeneratedProxy` with a **non-empty** recorded interface list. For
every other value, byte-for-byte the same arms run in the same order. And of the
three things the change does, one (the positive walk) can only ADMIT, so it
cannot turn a green *legal* row red; the two that can refuse more are gated on
that same entry condition.

So the enumeration reduces to: which rows have a proxy-shaped value? Three do.
The table gives all 27 anyway, because "reduces to" is the kind of claim that
should be checked row by row.

| row | shape | value's class / origin | why it cannot flip |
|---|---|---|---|
| s01 | `Marker[] <- String` | `java/lang/String`, BootImage | not a `GeneratedProxy`; gate never opens |
| s02 | `Marker[] <- Unrelated` | app class | as s01 |
| s03 | `SuperI[] <- Impl` | app class | as s01 |
| s04 | `Comparable[] <- Object` | `java/lang/Object` | as s01. Refused by the `ClassId(0)` arm `W7-101` §5 rewrote, far above this change |
| s05 | `Runnable[] <- String` | `java/lang/String` | as s01 |
| s06 | `CharSequence[] <- Integer` | `java/lang/Integer` | as s01 |
| s07 | `Map.Entry[] <- Integer` | `java/lang/Integer` | as s01 |
| s08 | `Serializable[] <- Object` | `java/lang/Object` | as s01 — **and note the new `comp_name == "java/io/Serializable"` allow is INSIDE the recorded-proxy block**, deliberately, precisely so this row and s09 cannot see it |
| s09 | `Cloneable[] <- Object` | `java/lang/Object` | as s08 |
| s10 | `Cloneable[] <- Integer` | `java/lang/Integer` | as s01 |
| s11 | `Runnable[][] <- String[]` | an ARRAY | returns in the array-vs-array block ~90 lines above the change; never reaches it |
| **s12** | **`Runnable[] <- Proxy(Marker)`** | **`$ProxyN`, `GeneratedProxy{[Marker]}`** | **the one row that moves, red → green.** `is_assignable_to_name(Marker, "java/lang/Runnable")` is false, component is neither `Serializable` nor a proxy super, arms 13/14 gated off, `synthetic_implements` declines → refused |
| s13 | `Marker[] <- Impl` | app class | not a proxy; and admitted by `is_subclass_of` long before the hatch region |
| s14 | `Marker[] <- Sub` | app class | as s13, via the superclass chain |
| s15 | `SuperI[] <- DeepImpl` | app class | as s13, via a super-interface |
| s16 | `Comparable[] <- Integer` | `java/lang/Integer` | as s13 |
| s17 | `Comparable[] <- String` | `java/lang/String` | as s13 |
| s18 | `Comparable[] <- MyComparable` | app class | as s13 |
| s19 | `Runnable[] <- lambda` | synthetic id `>= 0x8000_0000`, or a real `$$Lambda` with origin `GeneratedLambda` | either way not `GeneratedProxy`. The synthetic-id arm returns above the change; a real `$$Lambda` declares `implements Runnable` and is admitted by `is_subclass_of`. Its name contains no `$Proxy`, so it never depended on arm 13 |
| s20 | `CharSequence[] <- String` | `java/lang/String` | as s13 |
| s21 | `Serializable[] <- Integer` | `java/lang/Integer` | as s13 (`Integer → Number → Serializable`) |
| s22 | `Serializable[] <- Integer[]` | an ARRAY | as s11 |
| s23 | `Cloneable[] <- Integer[]` | an ARRAY | as s11 |
| s24 | `Marker[][] <- Marker[]` | an ARRAY | as s11 |
| **s25** | `Marker[] <- Proxy(Marker)` | `$ProxyN`, `GeneratedProxy{[Marker]}` | **the gate opens and the row still passes, on two roads.** `is_subclass_of` admits it before the change is reached (the class's `interfaces` vector holds `Marker`, from `interface_id_overrides`); if that ever declined, the new walk's first iteration is `is_assignable_to_name(Marker, "…$Marker")`, whose first test is `self.name == target_name` |
| **s26** | `Annotation[] <- annotation proxy` | real-JDK: `$ProxyN`, `GeneratedProxy{[Ann]}`. synthetic: `java/lang/annotation/AnnotationProxy` | real-JDK: `is_subclass_of` walks `$ProxyN → Ann → java/lang/annotation/Annotation` (javac writes that superinterface into `Ann`), and the new walk reaches the same node independently. synthetic: origin is not `GeneratedProxy`, so the `ends_with("AnnotationProxy")` term is ungated and behaves exactly as before |
| s27 | `Marker[] <- null` | — | a null element never reaches the predicate: the interpreter arm, `jit_aastore` and the x64 emitter each branch over it, and the predicate's contract says so |

**The count that matters:** `illegalRefused` 11/12 → 12/12, `legalAdmitted`
15/15 → 15/15, `fails` 2 → 0. PREDICTED.

### 5.1 The other scheduled-green vectors

`RJitArrayTypecheck`, `RArrayStoreTiers`, `RJdkViews`, `RExceptions` are green
and scheduled. Checked by reading the fixtures: **none of them stores a dynamic
proxy into a typed array.** The only proxy-adjacent array store anywhere else in
`regression-suite/src` is `RJdkReflBox`'s `seen[0] = a`, whose array is
`Object[][]` and whose value is an `Object[]` — an array value, which returns
far above this change, into a component that is itself an array descriptor.
`RArrayStoreTiers`' four interface rows (`s02`, `s03`, `s08`, `s09`) are
`Comparable[]`/`Runnable[]` against `Object`/`String`/`Integer`/lambda — the
`W7-101` population, untouched here.

### 5.2 The in-tree tests that pinned the old blanket

`vm/src/runtime/interpreter/tests.rs`'s
`aastore_refuses_a_real_mismatch_and_still_fails_open_where_it_must` asserts
twice that a `$Proxy`-named value fails open — against an interface component
and against a concrete one. **Both still pass, and not by luck.** Its fixture
proxy is `cm.try_ensure_synthetic_class("jdk/proxy3/$Proxy27", 0)`, i.e. a
FABRICATED class, whose origin is `GeneratedProxy { interfaces: [] }`. That is
the no-record shape, so `recorded_proxy_interface_set` answers `None` and arm 13
is ungated for it.

This is the cleanest thing about the discriminator and it was not designed for:
the population that test stands for — Spring's `TypeMappedAnnotation.adapt`
storing through `Array.set` in a VM that could not resolve the proxy's
interfaces — is *exactly* the population that keeps failing open, and the
population that HotSpot can adjudicate is exactly the one where this VM now can
too. The line falls in the same place from both directions.

## 6. Tests added

`vm/src/runtime/interpreter/typecheck.rs`, `mod g12_generated_proxy_interface_set`
— every class fabricated in-module, so no row can pass vacuously because a JDK
class failed to resolve. The proxy classes are given **no** `interfaces` vector
and **no** proxy superclass on purpose: `is_subclass_of` and
`is_assignable_to_name` therefore decline for them, so each admission below is
evidence about the arm under test rather than about the class graph.

* `an_empty_recorded_interface_list_is_no_record_at_all` — the discriminator,
  including the `is_empty` half and the `VmInternal` / `CompatibilityStub`
  negatives.
* `a_recorded_proxy_is_refused_by_an_interface_it_does_not_implement` — `s12`.
* `a_recorded_proxy_is_admitted_by_an_interface_it_does_implement` — `s25`, the
  control without which the one above would also pass against a predicate that
  had started refusing everything.
* `the_recorded_walk_is_transitive_and_directional` — both oracle rows
  (`SuperI[] <- proxy(SubI)` OK, `SubI[] <- proxy(SuperI)` throws). The second is
  what separates a walk from a name-pair match.
* `a_proxy_with_no_recorded_interface_set_still_fails_open` — the `$Proxy27`
  population.
* `every_proxy_is_serializable` — reaches the new arm rather than the hierarchy,
  because the fixture's proxy has no `java.lang.reflect.Proxy` superclass.
  `Cloneable` is the oracle-side control and is deliberately NOT asserted here:
  in a bare test VM the answer would turn on whether `java/lang/Cloneable`
  resolves rather than on this arm, and a row that can pass vacuously is not
  evidence.

`vm/src/jit/helpers.rs`, `mod g12_compiled_aastore_asks_the_shared_predicate` —
the legal direction driven end-to-end through `jit_aastore_type_check`, the
extern entry point compiled code actually calls (a `0` is the allow path and
needs no exception machinery), plus the refusal asserted at the predicate
`aastore_store_is_refused` consults two lines in. The refusal is deliberately
*not* driven through the extern function: reaching `i64::MIN` needs an installed
JIT thread AND a throwable built through `throw_runtime_error`, which are
`RExceptions`' subject and would make this test fail for reasons that are not
about `aastore`.

**Verified:** both files `rustfmt --edition 2021 --check`-parse, and neither
gained a formatting hunk — `typecheck.rs` has the same 3 pre-existing hunks
before and after, `helpers.rs` the same 32 (compared against `git show HEAD:`).
Zero CR bytes in both. No duplicate `fn` name introduced in either file.

**Not verified, and this is the honest limit of the record:** nothing was
compiled and nothing was run. `HANDOFF-20260814` §5 — *a green build proves you
broke nothing, not that you did something* — cuts both ways here, and this lane
has neither half.

## 7. `W7-101` and `W8-C10-1` — can they close?

**`W7-101-aastore-interface-component-blanket.md`: YES, close it.**

Its subject is arm 11, the `if comp.is_interface() { return true }` blanket, and
its companion arm 7, the `ClassId(0)` fail-open. Both are gone from the source,
and — this is the part it was owed and never had — **both are now MEASURED
fixed, not predicted.** `RArrayStoreInterfaces` `s01`–`s11` are the interface
component family it was opened for, and all eleven refuse correctly on the
binary built from `d87dff06a`. Its own §6 named the falsification condition
("if `s08`/`s09` go red, the deletion was not the right fix"): `s08` and `s09`
are green, along with `s21`, `s22`, `s23` — so `W8-C16-1`'s
`Serializable`/`Cloneable` split, which `W7-101` §7 deferred, is measured good
too. `W7-101` should close with a **FIXED-MEASURED** banner citing this run, and
with the correction that its §5 fix was *necessary and not sufficient*: the
family it closed had a third fail-open below it that its own §4 list of "five
more precise successors" counted as a successor rather than as a suspect. Hatch
13 in that list is this record's defect.

**`W8-C10-1-typecheck-hatch-audit-and-aastore-precedence.md`: NO, not yet — and
the reason is a finding, not bookkeeping.**

Three things are still open in it, and only one is now answerable:

* **§2, hatch 13 (`$Proxy` / `AnnotationProxy` name test) — verdict CHANGED by
  this record.** The audit read it, asked its three questions, and answered
  "successor below it? none — KEPT". That answer was wrong, and instructively
  so: **the audit only looked DOWN.** Its whole method is "is there a more
  precise arm further down this function", and hatch 13's successor is not an
  arm at all — it is `ClassOrigin::GeneratedProxy`, a field on the class that
  did not exist when the hatch was written. Hatch 14 is the same. **That is the
  correction `W8-C10-1` should carry before it closes**: a fail-open hatch can
  be superseded by data as well as by code, and a downward-only audit cannot see
  it. Hatches 12 and 15 were re-examined here under that widened question and
  both survive it — a synthetic id `>= 0x8000_0000` genuinely has no record
  anywhere, and `synthetic_implements` is a name fallback for classes that by
  definition have no interface data.
* **§6, the inverted `aastore` exception precedence** (`ArrayStoreException`
  where HotSpot gives `ArrayIndexOutOfBoundsException`, interpreter side) — its
  N1 nomination targets `vm/src/runtime/interpreter/opcodes.rs`, which is not
  this lane's file and which this lane did not touch. Unknown whether it has
  landed; `RArrayStoreTiers` `s15` is the vector.
* **§4, the residual `synthetic_implements` over-admissions** — partly closed by
  `W8-C16-2`, with a measured residual of 50 cells.

So `W8-C10-1` stays open, with hatch 13's verdict flipped from **KEPT** to
**CHANGED (G12-1)** and hatch 14's from an unlisted pass to **CHANGED (G12-1)**.

## 8. What this lane did NOT do

Stated because a record that only lists what moved reads as a wider claim than
it is.

* **Did not build, run, or measure anything on CratonVM.** Every "after" in §5
  is PREDICTED. The oracle side is measured; the VM side after the change is not.
* **Did not touch the JIT's behaviour.** `vm/src/jit/helpers.rs` gained a doc
  comment and a test module. If `s12` moves in `cold` but not in `hot`, this
  record's §4.1 caller enumeration is wrong and that is where to look first.
* **Did not widen any refusal beyond generated proxies with a recorded,
  non-empty interface set.** Every other fail-open in `aastore_element_assignable`
  is left exactly as it was, including the ones §7 says were re-examined and
  survived — hatch 12 (synthetic ids), hatch 15 (`synthetic_implements`), the
  `CompatibilityStub` `Serializable`/`Cloneable` arm, and the `ClassId(0)`
  reclaimed-hole arm. That is deliberate per the brief: refuse only what is
  positively provable.
* **Did not verify that `is_subclass_of` alone already admits `s25`.** It should
  — `interface_id_overrides` puts `Marker`'s exact `ClassId` in the proxy class's
  `interfaces` vector — but "should, by reading" is not "does", and this lane
  could not run the VM. That is precisely why the new walk is redundant with it
  (§4). If `s25` ever goes red, the redundancy is what failed, and both roads
  need looking at.
* **Did not touch `class_chain_reaches_proxy_instance`, `synthetic_implements`,
  `proxy_instance_satisfies_target`, or `annotation_proxy_satisfies_target`.**
  Only the `aastore` call sites of the first were scoped. The `checkcast` /
  `instanceof` / dispatch answers for a proxy are **unchanged** — this record
  says nothing about whether `proxy instanceof Runnable` is right, and it is a
  different predicate with different callers.
* **Did not examine the `--real-jdk` / synthetic-JDK split for this arm beyond
  the origin.** In synthetic-JDK mode a proxy that reaches the degrade path is a
  `Proxy$Instance` shim with `VmInternal` origin, which answers `None` and keeps
  the old behaviour; that is reasoned, not measured.
* **Did not renumber, reindex, or edit `INDEX.md` / `README.md`** (shared), or
  any of the six other lanes' files.

## 9. Residual risk, named

**Spring / Byte Buddy / annotation-heavy workloads outside the differential
suite.** A real generated proxy stored into an array of an annotation or
interface it does *not* implement is now refused where it used to be admitted.
That change matches HotSpot exactly — such a store throws on a real JVM — so any
application it breaks was already broken on a real JVM at that line. But it is
the one behavioural direction this change opens, it is not covered by any
scheduled vector, and `Array.set` (§4.1) carries it into reflective code too.
If something Spring-shaped starts throwing `ArrayStoreException`, this is the
record.

The bounded version of the same statement: the refusal requires the VM to hold a
non-empty recorded interface set for the value's class, which today happens on
exactly one path — `define_or_get_proxy_class` succeeding with
`ProxyClassOutcome::Real`. Every degrade, every failure, every fabrication and
every synthetic-JDK proxy still fails open.

## 10. NOMINATIONS

### N1 — `vm/src/runtime/interpreter/tests.rs`: the comment, not the assertion

Both `$Proxy` assertions in
`aastore_refuses_a_real_mismatch_and_still_fails_open_where_it_must` still pass
(§5.2) and **must not be changed**. What is now under-described is the comment
above them, which says the fail-open population is served by "the `$Proxy` name
test below `is_subclass_of`" without saying *which* proxies that test still
serves. Suggested amendment to the comment block at ~:100:

> …now by the arm that can actually tell a proxy from an `Object`: the `$Proxy`
> name test below `is_subclass_of`. **That arm is scoped as of G12-1 — it serves
> a proxy whose interface set the VM does NOT hold, which is exactly this
> fixture's `jdk/proxy3/$Proxy27` (`try_ensure_synthetic_class` gives it
> `ClassOrigin::GeneratedProxy { interfaces: [] }`). A proxy from
> `Proxy.newProxyInstance` has a RECORDED set and is adjudicated against it, so
> this assertion is about the fabricated population specifically and would not
> hold for a real generated `$ProxyN`.**

The same file's `:91` still cites `W7-39` for what is now `W7-101` — already
INDEX N1, restated here so it is not lost.

### N2 — `native-builtins/src/reflect_annotations.rs`: state the origin, do not re-derive it

`define_or_get_proxy_class` builds `DefineClassFull` with `..Default::default()`
and no origin, so `ClassOrigin::GeneratedProxy { interfaces }` is reconstructed
by `classify_defined_origin`'s **name heuristic**
(`is_generated_proxy_name`, which requires a digit-suffixed `$ProxyN` simple
name). It works today — `build_proxy_spec_for` emits `format!("{pkg}/$Proxy{n}")`
— and this fix now depends on it.

`DefineClassFull` has no `origin` field, and `classify_defined_origin` gives
`options.origin` top precedence, so the change is a pair: add
`pub origin: Option<ClassOrigin>` to `DefineClassFull`
(`native-api/src/registry.rs:293`), thread it to `DefineClassOptions`, and have
`define_or_get_proxy_class` pass
`ClassOrigin::GeneratedProxy { interfaces: <the same list it already computes for
interface_id_overrides> }`. The generator knows the answer; deriving it from the
class's own name is the shape this directory keeps filing records about.

Not urgent — the heuristic and the override would agree today. It removes a
silent coupling between a naming convention and a type decision.

### N3 — `regression-suite/src/RArrayStoreInterfaces.java`: three rows the family is missing

Read-only for this lane. The fixture's 27 shapes contain **one** proxy-refusal
row (`s12`) and two proxy-admission rows, which is enough to catch the defect
but not enough to characterise the rule. Measured on the oracle (§2) and
currently unasserted:

* `Cloneable[] <- Proxy(Marker)` → `ArrayStoreException`. **The most valuable of
  the three**: paired with the row below it, it is what distinguishes "inherited
  from `java.lang.reflect.Proxy`" from "proxies satisfy marker interfaces", and
  a VM that got it wrong would look correct on every other row here.
* `Serializable[] <- Proxy(Marker)` → no-throw.
* `SuperI[] <- Proxy(SubI)` → no-throw, with `SubI[] <- Proxy(SuperI)` →
  `ArrayStoreException` beside it. Directionality; `s15` covers it for an
  ordinary class but nothing covers it for a proxy.

All three keep the fixture's pairing discipline (each legal store beside an
illegal one into the same array) and none has an unstable message, so all three
can be asserted on kind AND published on a `CK ` line. Note the denominators are
counted off `EXPECTED_KIND` rather than written as literals, so adding rows does
not require touching the balance line.

### N4 — `vm/src/runtime/interpreter/opcodes.rs`: `W8-C10-1` §6/N1 is still open here

Restated, not re-derived: the interpreter's `Aastore` arm runs NPE → ASE →
AIOOBE where JVMS §6.5 fixes NPE → AIOOBE → ASE, and `jit_aastore` already has
the right order. Vector `RArrayStoreTiers` `s15`. Unknown whether it has landed;
this lane did not touch the file and could not run the vector.

### N5 — `vm/src/runtime/interpreter/dispatch_virtual.rs:478`: F32-1's drifted twin, untouched

The inlined proxy-super walk that matches `Proxy$Instance` only, missing the
default real super. Already `F32-1`; noted here because this record's §3.1 makes
`class_name_is_proxy_super` load-bearing in a second place, and a lane that
narrows that predicate to fix one site would now break `aastore` as well.

## 11. What I could not settle

* **Whether `s25` and `s26` are green today because of the hierarchy walk or
  because of the blanket.** Both produce `no-throw` and no in-tree instrument
  separates them. The fix is built so that it does not matter (§4, §8), but the
  question is open and a `--dump-class-origins` row for the generated `$ProxyN`
  plus its `interfaces` vector would close it in one run.
* **Whether the annotation `$ProxyN` in `s26` takes the real-proxy path or the
  `AnnotationProxy` path in the binary that was measured.** The code has both and
  the row is green either way; the fix handles both; which one runs is a
  `--dump-native-registry` question, and `HANDOFF-20260814` §4 is explicit that
  reading cannot settle it.
* **Whether `s12`'s ASE message will match.** It is not asserted (`coldSkip(12)`)
  and never reaches the diff, so it cannot make the vector red — but it also
  cannot be checked, and CratonVM's `jdk.proxyM.$ProxyN` numbering need not agree
  with HotSpot's.
* **The blast radius on non-suite workloads** (§9). Named, bounded, unmeasured.
