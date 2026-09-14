# H18-1 — the opcode, the reflection and the exception message read three different classes off one object, and the fix is one table with three callers

**Status: FIXED IN SOURCE, NOT COMPILED.** This lane may not build. Landed at
`a73aea08b` on `claude/jdk-only-mode-handoff-09b48c`. Everything measured below
was measured on the **pristine** round-5 binary, which does **not** contain the
patch. §7 is the verification agenda the next lane owes.

**Date** 2026-08-21
**Lane** H18
**Subject** `H15-2` (diagnosis), `H0-2` §4/§5 (the twelve cells), `H15-1` (the
five standing failures)
**Binary** `C:/craton/cratonvm-r5.exe` @ `9eef86699` — prebuilt, pre-patch
**Oracle** HotSpot 25.0.3+9, `/c/Program Files/Microsoft/jdk-25.0.3.9-hotspot`
**Probe** printed in full in `H18-3` §1 — deliberately not filed under
`regression-suite/probes/`, which is outside this lane's ownership. (The one
that *is* filed there, `OpcodeVsReflectionProbe.java`, does not compile: see
`H18-2` §4.)

Claims are **MEASURED** (ran it) or **ARGUED** (read it).

---

## 1. The worktree gap, stated first

My worktree was cut at `26e4b5db4`; the branch tip was `9eef86699`. **Eight of
eight lanes have now hit this and I was the ninth.** The gap was not small: it
was the entire round-5 wave — H9's `HashSet.PRESENT` fix, H10's tier
differential, H11's 237 fabricated abstract rows and the four retired
`DataInput`/`DataOutput` registrations, H12's three-door direct-bind matrix,
H13's CHM `size()=1`-with-an-empty-`keySet` contradiction, H15's three records,
two JCA provider fixes (`ca8f03069`, `b773038e2`), the untyped-alloc ratchet and
its CI job, and the round-5 baseline that moved the corpus denominator to 105.
Reading `H15-2` before `git merge --ff-only` would have given me the version
*without* its INDEPENDENT REPRODUCTION section, which is the half that contains
the seven-of-seven table.

## 2. The finding, in one line

**One object. One instant. Three doors. Three different answers.**

| door | what it says about `Map.of("k","v")` | mechanism |
|---|---|---|
| `Class.isInstance(AbstractMap)` | **true** (right) | consults `getclass_display_class_id` |
| `instanceof AbstractMap` (opcode) | **false** (wrong) | walks the stamp's real chain, which is `Object` |
| the `ClassCastException` it then throws | *"class **java.util.ImmutableCollections$Map1** cannot be cast to class java.util.AbstractMap"* | consults `cce_display_class_name` |

`H15-2` found the first two. The third is new, and it is the sharpest statement
of the defect in the tree: **the VM refuses the cast using the stamp, and then
uses the display alias to write a message that is a false sentence.**
`ImmutableCollections$Map1` *is* an `AbstractMap` — HotSpot's own class file
says so (§4.2). The VM has the correct answer in hand, at the same call site, in
the same microsecond, and spends it on the error text instead of the decision.

## 3. Reproduction — third and fourth independent runs

**MEASURED**, `cratonvm-r5.exe`, Compatible mode, two runs, byte-identical:

```
A Map.of()                       AbsMap=F/T* AbsColl=F/F  RandAcc=F/F   getClass=java.util.ImmutableCollections$MapN
A Map.of(k,v)                    AbsMap=F/T* AbsColl=F/F  RandAcc=F/F   getClass=java.util.ImmutableCollections$Map1
A Map.copyOf                     AbsMap=F/T* AbsColl=F/F  RandAcc=F/F   getClass=java.util.ImmutableCollections$Map1
A List.of()                      AbsMap=F/F  AbsColl=F/T* RandAcc=F/T*  getClass=java.util.ImmutableCollections$ListN
A List.of(1,2,3)                 AbsMap=F/F  AbsColl=F/T* RandAcc=F/T*  getClass=java.util.ImmutableCollections$ListN
A Set.of(x)                      AbsMap=F/F  AbsColl=F/T* RandAcc=F/F   getClass=java.util.ImmutableCollections$Set12
A Collections.unmodifiableList   AbsMap=F/F  AbsColl=F/F  RandAcc=F/T*  getClass=java.util.Collections$UnmodifiableRandomAccessList
```

`T`/`F` is `instanceof`/`isInstance`; `*` marks a disagreement. **Seven of seven
receivers, nine divergent cells.** HotSpot answers `T/T` for every starred cell.

**`--jdk-only` is byte-identical to HotSpot on every section of this probe** —
sections A, B, C *and* D. Strict mode never allocates these stamps
(`vm_init.rs:1810`: `ensure_bootstrap_compat_class` returns `None` under
`--jdk-only`, so the `for` loop `continue`s past all eleven), so there is
nothing here for a strict-mode change to earn. This is a **Compatible-mode
defect on all three faces**, exactly as `H15-1` characterises the five standing
failures.

### 3.1 The trap I inherited and re-checked rather than trusted

`H15-2`'s reproduction note says its first probe tested *interfaces* (`Map`,
`List`, `Set`) across nine receivers, found **zero** divergence, and nearly
published "not reproduced". I kept the interface cells in my own probe as a
control and they are indeed all clean — `unmodifiableCollection instanceof
Collection`, `keySet instanceof Set`, `unmodifiableSortedSet instanceof
SortedSet` are `T/T` on both VMs. The stamps carry an **accurate interface
list** (`vm_init.rs:1746-1790`) and a **fabricated superclass** (`:1813`,
`set_superclass(cid, Some(object_id))`). A probe that asks only about interfaces
is asking about the half that was done correctly.

## 4. Why the size does not matter — measured, not argued

`H15-2` §5.2 proposes a family-level exit that never asks for the collection's
size, and asserts the size cannot change the answer. That assertion is the load
bearing beam of the whole fix, so I did not inherit it. **MEASURED** with the
oracle's own `javap`:

```
final class ImmutableCollections$Map1  extends ImmutableCollections$AbstractImmutableMap  implements Serializable
final class ImmutableCollections$MapN  extends ImmutableCollections$AbstractImmutableMap  implements Serializable
final class ImmutableCollections$List12 extends ImmutableCollections$AbstractImmutableList implements Serializable
final class ImmutableCollections$ListN  extends ImmutableCollections$AbstractImmutableList implements Serializable
final class ImmutableCollections$Set12 extends ImmutableCollections$AbstractImmutableSet  implements Serializable
final class ImmutableCollections$SetN  extends ImmutableCollections$AbstractImmutableSet  implements Serializable
```

Every size pair is **supertype-identical**: same superclass, same interface
list, nothing else declared. And the ancestors carry the cells that matter:

```
abstract class ImmutableCollections$AbstractImmutableList extends …AbstractImmutableCollection implements List, RandomAccess
abstract class ImmutableCollections$AbstractImmutableCollection extends java.util.AbstractCollection
abstract class ImmutableCollections$AbstractImmutableSet extends …AbstractImmutableCollection implements Set
abstract class ImmutableCollections$AbstractImmutableMap extends java.util.AbstractMap
```

**`RandomAccess` sits on `AbstractImmutableList`, not on `List12`/`ListN`.** So
even the one cell with a performance contract attached is size-independent. The
family exit is not a pragmatic approximation; it is exact for every subtype
question that can be asked.

### 4.1 The negative control, from the same source

```
class Collections$UnmodifiableRandomAccessList extends Collections$UnmodifiableList implements RandomAccess
class Collections$UnmodifiableList            extends Collections$UnmodifiableCollection implements List
```

`Collections$UnmodifiableCollection` extends `Object`. So
`Collections.unmodifiableList(…) instanceof AbstractCollection` must stay
**false** while `… instanceof RandomAccess` must become **true** — the two
halves of one receiver splitting in opposite directions. That is the control an
over-broad patch breaks first, and it is decided by the class files rather than
by my judgement.

## 5. The hazard, and why the fix does not have it

The task named one hazard and asked that it not be papered over:
`getclass_display_class_id` runs `invoke_virtual(this, "size", "()I")` and can
move the heap mid-opcode.

**The hazard is real, and it is worse than "the receiver is a bare local".**
`op_instanceof` and `op_checkcast` *do* pin around `resolve_class_loader_aware`
— and then **release the pin before the disjunction runs**
(`opcodes.rs:2267-2268`, `:2457-2458`: `truncate(pin)` precedes the six-way
predicate chain). So the receiver is unrooted at exactly the point a new
predicate would go.

And the VM says so itself. **MEASURED** — the round-5 binary's own root guard,
fired by this probe's checkcast section:

```
ERROR cratonvm::gc::guard: …root COLLECTION gap, not a mark or sweep one.
  obj="0x208fdcc8d08" site="checkcast" in_published_snapshot=false published_roots=0
  holder=frame#1 H18Probe.castToAbstractMap pc=4 local[0] kind=0 live=true frames=2
```

I am not claiming this guard proves a collection would have lost the object —
`collections_now=0`, nothing had run. I am claiming something narrower and
sufficient: **the VM's own instrument reports the checkcast receiver as absent
from the published root snapshot, at `site="checkcast"`, unprompted.** That is
the wrong place to introduce a call that runs Java.

The patch therefore never asks for the size (§4 shows it does not need to), and
the only operation left that can safepoint — loading the display class on first
use — is wrapped in a `native_pin_roots` entry with the reference refreshed
afterwards. That is why `display_class_satisfies_target` takes `&mut ObjectRef`
rather than `ObjectRef`: **`op_checkcast` reads the receiver again in its
failure path** (`class_id_of`, then `cce_display_class_name`), so a move the
caller did not observe would corrupt the very exception this record is about.

### 5.1 Why it loads rather than declining on "not loaded yet"

The cheaper design — resolve via `get_loaded_class_id`, decline if absent —
needs no pin at all. I rejected it. `Map.of()` is served by a native in
Compatible mode, so nothing necessarily loads `java.util.ImmutableCollections`
before the first `instanceof`; but `getclass_resolve_name` **does** load it on
the first `getClass()`. Declining on absence would make `x instanceof
AbstractMap` depend on whether anything had called `x.getClass()` earlier in the
program. **An order-dependent `instanceof` is a worse defect than a consistently
wrong one**, and it is the kind that costs a week to bisect.

### 5.2 The lock, which is the part that would actually have deadlocked

The six existing predicates are the operands of one `||` chain whose first term
is `shared.classes.class_manager.read().is_subclass_of(…)`. The
`RwLockReadGuard` temporary from that `.read()` lives **to the end of the
statement**, i.e. across every later operand. Appending an arm that takes the
class-manager **write** lock — which loading does — would have self-deadlocked
against a non-reentrant `parking_lot::RwLock` on the first cold display class.

`typecheck.rs`'s `resolve_component` already documents this exact trap at
length. The fix is the same one: bind the existing chain to a `let` so the guard
drops at the semicolon, then evaluate the new arm as a separate statement. **The
`let` in both opcodes is load-bearing, not stylistic**, and it is commented as
such so nobody "simplifies" it back.

## 6. What the patch is, and what it is not

Three files, all inside this lane's ownership.

**`typecheck.rs`** — `unmod_stamp_display_name` (the stamp→JDK-class table at
family granularity) and `display_class_satisfies_target` (the predicate).
**`opcodes.rs`** — one term appended to both disjunctions, last.
**`lambda.rs`** — `cce_display_class_name` had this same table **inlined for
maps only**; it now reads the shared one. See `H18-2`.

### 6.1 The "second copy" objection is spent — but not for the reason H15-2 gives

`H0-2` §5 declined this fix as "the second copy of one rule". `H15-2` §5.1
retired that objection by arguing the patch would be *a second caller, not a
third copy*, since `getclass_display_class_id` already exists in
`native-builtins` and `vm` already depends on that crate.

**That argument is sound but its premise is incomplete, and I found the missing
piece by grepping rather than by reading `H15-2`.** A second in-VM copy of this
rule *already existed*: `cce_display_class_name`
(`vm/src/runtime/interpreter/lambda.rs:284`), whose own doc comment says *"Keep
this mapping in lockstep with `native-builtins`' `getClass()` mapping for
maps."* It is map-only, it is pure Rust, it reads the backing map's `size`
**field** rather than calling `size()`, and **`op_checkcast` already calls it**.

So the real count was never one copy. It was two, one of them one-eleventh
complete, sitting in the file the fix had to touch anyway. What landed grows
that copy to cover all eleven stamps and gives it a second and third caller.
**Net copies: still two.** And it buys something the `native-builtins` route
could not: the decision and the message now read the *same* function, so the
divergence in §2's table is closed structurally rather than by keeping two
tables in step.

A consequence worth stating plainly, because it looks like a bug: for a
`Map.of("k","v")`, `getClass()` reports `Map1` while
`unmod_stamp_display_name` returns `MapN`. That is deliberate and harmless —
§4 proves the two are supertype-identical — and the *message* path still refines
to `Map1` because a message wants the size and a subtype answer does not.

### 6.2 What this does not do

It does not make the containers real. `H0-2` §5's principled fix and this one
are not alternatives: this is the interim, and it becomes dead code the day
`Map.of` returns a genuine `ImmutableCollections$Map1`. `H15-2` N1 says the same
and is hereby seconded.

**It also does not reach compiled code.** See `H18-3` — the three tier twins are
outside this lane's ownership, do not have the arm, and one of them is on the
path that decides `Collections.binarySearch`'s algorithm once that method is
hot. That is the largest caveat on this record.

## 7. Predictions, each with its falsifier

**PREDICTED**, none verified — this lane cannot build.

1. **`RImmutableFactoryTypes` goes GREEN**, taking `SUITE=core` to **65/65** and
   `SUITE=all` to **101/105**. It is the only one of the five standing failures
   whose complaint is a subtype cell, so it is the only one I expect to move.
   *Falsifier:* it still reports `Map.of(k,v) must be instanceof AbstractMap`,
   which means `display_class_satisfies_target`'s screen never matched — check
   by temporarily removing the `starts_with("cratonvm/internal/Unmodifiable")`
   test.
   **A vector going green here is a fix, not a violation** (`H15-1`: all five
   pass under `--jdk-only` and fail only in Compatible mode).
2. **The other four standing failures do NOT move.** `H15-3` attributes three of
   them to stand-in classes (`Function$AndThen` and friends) and none to a
   subtype cell. *Falsifier:* any of them changing state means this patch
   reaches further than its screen should allow, and that is a bug report, not a
   bonus.
3. **`--jdk-only` is unmoved at 105/105.** The screen is false for every
   receiver in strict mode because the stamps are never created there —
   independently confirmed in §3 by the probe, not inferred from a green arm.
   *Falsifier:* **any** movement in the strict arm, in either direction.
4. **The §3 probe answers `T/T` on all seven rows**, and
   `Collections.unmodifiableList(…) instanceof AbstractCollection` stays
   **false**. The `RandomAccess` cells are asserted by no vector, so this one
   must be run by hand.
5. **`RCollections`, `RJdkMapViews`, `RChmKeySetView`, `RJdkViews` stay green.**
   They exercise these carriers hardest.

## 8. What I did NOT verify

* **It does not compile.** No `cargo build`, `check`, `test` or `clippy` was
  run — this lane is forbidden to. Every borrow, every type and every name in
  the patch is argued from the tree, not from `rustc`. The likeliest failure
  modes, in order: `JvmThread` not actually re-exported into `typecheck.rs`'s
  `use super::*` scope (it is named nowhere else in that file); the closure
  `immutable` borrowing `shared` in a way that conflicts with a later use; and
  `Class::name` not being `Deref<Target=str>` for the `starts_with` call.
* **No arm was run.** `--jdk-only`, `SUITE=all` and `SUITE=core` numbers in §7
  are predictions.
* **The JIT twins are untouched and unmeasured.** `H18-3`.
* **Throughput of the new arm is not measured.** §6's cost claim (one read
  guard, one prefix compare) is a reading of the code, not a profile.
* **The four iterator/entry stamps** (`UnmodifiableItr`, `UnmodifiableListItr`,
  `UnmodifiableEntryItr`, `UnmodifiableMapEntry`) return `None` and were never
  probed for subtype divergence at all. Stated so it is not read as covered.
* **`Collections.reverse`/`shuffle`/`fill`/`copy`/`swap`** were not measured;
  only `binarySearch` was (`H18-3` §2). The other five are asserted from the JDK
  source's branch, not from a run.

## NOMINATIONS

**N1 — port the arm to the three tier twins.** `vm/src/jit/helpers.rs:8502` and
`:8679` and `vm/src/vm/vm_exec.rs:20309` end their disjunctions at
`synthetic_implements_public`. Until they get the arm, `x instanceof
RandomAccess` answers **true** interpreted and **false** compiled — and
`Collections.binarySearch` is precisely the shape that gets hot. The patch and
the reasoning are in `H18-3` §3. **This is the highest-value item on this page**
and it is the one thing that decides whether §2's 886x actually goes away.

**N2 — `RImmutableFactoryTypes` still asks 1 of 12 cells** (`H0-2` N1, unclosed
after two waves). It reports 219 checks and one divergence; nine of the cells
this record measures are invisible to it. Nine lines in a vector that already
has the receivers. Until then, a partial regression of this fix reads as green.

**N3 — the `RandomAccess` row deserves a check with a cost attached**, not just
a value assertion (`H0-2` N2, also unclosed). §2 of `H18-3` gives a within-VM
A/B that needs no HotSpot column and no wall-clock threshold: assert
`binarySearch(unmodifiableList(x))` is within a small multiple of
`binarySearch(x)` **in the same run**. Host noise cancels in the ratio, which is
the only reason such a check can live in a gate on this host.
