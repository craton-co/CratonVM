# `aastore` fails open on every interface-component array

**Status: OPEN. Diagnosed and measured against the oracle; NOT fixed.**
The fix is a nomination (§4), not a change — the file lives outside the owning
lane's boundary. Six lines.

Filed 2026-08-12. Sibling of `W7-38-jit-aastore-never-called-its-own-check.md`,
found while auditing the check that record wires up. The two are independent:
W7-38 is "the check is never reached in compiled code", this is "the check
itself declines to answer for a whole family". Fixing either alone leaves the
other live.

---

## 1. The blanket

`vm/src/runtime/interpreter/typecheck.rs`, in `aastore_element_assignable` —
the predicate shared by the interpreter's `aastore` opcode and the
`jit_aastore` helper:

```rust
// Component is an INTERFACE → fail open. Proving a value implements an
// interface is unreliable in this VM (dynamic proxies, annotation
// proxies, and synthetic classes implement interfaces at runtime / by
// name, invisibly to the static hierarchy). A genuine ArrayStoreException
// essentially always involves a concrete-class component (Number[],
// String[], …); for an interface[] we don't risk a spurious throw.
if cm.get_class(comp_id).map_or(false, |c| c.is_interface()) {
    return true;
}
```

Any store into any array whose component type is an interface is permitted,
unconditionally, without consulting the value at all.

## 2. What the oracle says

HotSpot 25.0.3 (Microsoft build 25.0.3+9-LTS), measured, not recalled:

| shape | HotSpot | blanket answers | verdict |
|---|---|---|---|
| `Comparable[] c = new Comparable[1]; c[0] = Integer.valueOf(1)` | **OK** | allow | correct, by luck |
| `Object[] c = new Comparable[1]; c[0] = new Object()` | **ArrayStoreException: java.lang.Object** | allow | **WRONG** |
| `Object[] r = new Runnable[1]; r[0] = "s"` | **ArrayStoreException: java.lang.String** | allow | **WRONG** |
| `Runnable[] r = new Runnable[1]; r[0] = () -> {}` | **OK** | allow | correct |

The blanket is not a rounding error on a rare shape. It is *the whole family*:
it gets the legal stores right for the same reason it gets the illegal ones
wrong — it never looks.

### The brief that started this was wrong about the interesting case

The task that produced this record asserted that
`Comparable[] c = new String[1]; c[0] = Integer.valueOf(1)` **succeeds**,
because "Integer IS Comparable", and named it the case a naive fix breaks. It
throws:

```
ASE   | 4 Comparable[] = new String[1] <- Integer | java.lang.Integer
```

The static type of the variable is irrelevant. JVMS §6.5 *aastore* checks
against the array's **actual runtime component type**, which here is `String`,
and `Integer` is not a `String`. The brief stated that rule correctly two
paragraphs earlier and then contradicted it — which is why the rule is worth
restating rather than paraphrasing: *the declared type of the reference you
store through never enters the check.*

The case that genuinely is delicate is row 1: the component type really is the
interface `Comparable`, and `Integer` implements it. A fix that reads "is the
value's class a **subclass** of the component class" breaks that, because
`Integer` is not a subclass of `Comparable`, it *implements* it. That is the
trap, and it is a different shape from the one the brief described.

## 3. Why the stated rationale no longer holds

The comment's justification is sound in itself: dynamic proxies, annotation
proxies and synthetic classes acquire interfaces at runtime, invisibly to the
static hierarchy, so a naive interface check would throw spuriously on them.

But **every one of those cases is already handled by code that sits below the
blanket and can never be reached because of it.** In source order, after the
blanket:

1. `cm.is_subclass_of(value_class_id, comp_id)` — and this **does** walk
   implemented interfaces, transitively through super-interfaces
   (`classloading/src/class.rs`, `is_subclass_of_inner`, the
   `for &iface_id in &self.interfaces` loop). It answers row 1 correctly on its
   own. The blanket is not protecting a check that cannot handle interfaces; it
   is short-circuiting one that can.
2. `value_class_id.as_u32() >= 0x8000_0000` — synthetic lambda/proxy ids, fail open.
3. the `$Proxy` / `AnnotationProxy` name test — fail open.
4. `class_chain_reaches_proxy_instance(...)` — fail open.
5. `synthetic_implements(shared, value_class_id, comp_name)` — the name-based
   fallback for synthetic classes, fail open.

Hatches 2–5 are *exactly* the populations the blanket's comment cites, and they
were added after it. So the blanket is now redundant with its own justification:
it is an early return that pre-empts five more precise successors, and the only
thing it still contributes is the false negatives in §2.

This is the general shape worth carrying: **a fail-open guard written before its
precise replacements existed does not become harmless when they arrive — it
becomes the reason they never run.** The comment kept reading as current because
everything it says about proxies is still true; what changed is that it is no
longer the only thing standing between them and a spurious throw.

## 4. Nomination

`vm/src/runtime/interpreter/typecheck.rs`. Delete the blanket and let control
reach `is_subclass_of` and the five hatches.

**old** (exact literal):
```rust
        // Component is an INTERFACE → fail open. Proving a value implements an
        // interface is unreliable in this VM (dynamic proxies, annotation
        // proxies, and synthetic classes implement interfaces at runtime / by
        // name, invisibly to the static hierarchy). A genuine ArrayStoreException
        // essentially always involves a concrete-class component (Number[],
        // String[], …); for an interface[] we don't risk a spurious throw.
        if cm.get_class(comp_id).map_or(false, |c| c.is_interface()) {
            return true;
        }
```

**new** (exact literal):
```rust
        // An INTERFACE component is NOT a reason to fail open. This used to
        // return `true` unconditionally here, citing dynamic proxies,
        // annotation proxies and synthetic classes — populations that acquire
        // interfaces at runtime, invisibly to the static hierarchy. Every one
        // of those is now handled by a more precise successor BELOW this point
        // (the synthetic-id range test, the `$Proxy`/`AnnotationProxy` name
        // test, `class_chain_reaches_proxy_instance`, and
        // `synthetic_implements`), all of which were added after the blanket
        // and none of which could ever run while it stood. Meanwhile
        // `is_subclass_of` walks implemented interfaces transitively, so it
        // answers the legal case — `Comparable[] <- Integer` — on its own.
        //
        // What the blanket cost was the whole illegal family: HotSpot 25.0.3
        // throws `ArrayStoreException` for `Comparable[] <- Object` and
        // `Runnable[] <- String`, and this predicate permitted both.
        // See docs/known-issues/jdk-only/W7-39-aastore-interface-component-blanket.md.
```

The fail-open posture the function documents is preserved — it is simply
delivered by the five hatches that were written for it, instead of by a test
that cannot tell a proxy from an `Object`.

## 5. Vector

`regression-suite/src/RArrayStoreTiers.java` (new, filed with W7-38) already
carries the four interface shapes as `s02`, `s03`, `s08`, `s09`, so this record
needs no new fixture — but note **`s02` and `s03` are red on this defect
regardless of W7-38**, in *both* tiers, including under `--nojit`. That is the
discriminator between the two records:

* red only without `--nojit` → W7-38 (the compiled tier never reaches the check)
* `s02`/`s03` red **with** `--nojit` → W7-39 (the check itself fails open)
* both → both, which is the expected state of the tree until this record is fixed

**Before:** not measured on a CratonVM binary this wave — no build was run.
Read from source; the blanket is unconditional, so `s02`/`s03` are predicted red
in both tiers.
**After (PREDICTED):** `s02`/`s03` green in both tiers, `s08`/`s09` still green
(the case `is_subclass_of` must keep answering correctly — if those go red, the
deletion was not the right fix and the hatch below it is missing a population).
