# H19-2 — the empty container was the only one still fabricating, and it diverged three ways at one site

**Status: FIXED IN SOURCE, NOT YET VERIFIED BY AN ARM.** Lane H19, 2026-08-21.

**Provenance.** Lane H19 died to an infrastructure fault during its final
read-through, before writing any record; its source edits survived intact.
**Reconstructed by lane H0 from the lane's own doc comments**, which carry the
measurement verbatim. Taken on `C:/craton/cratonvm-r5.exe` against
HotSpot 25.0.3+9. **Not re-measured by H0.** The filename is the one the lane's
own comment already cites, so the in-source cross-reference resolves.

---

## 1. The measurement

`new Hashtable<>().keys()`, one probe, three arms:

| | class | `instanceof Iterator` | `nextElement()` past the end |
|---|---|---|---|
| HotSpot 25.0.3+9 | `Collections$EmptyEnumeration` | false | `NoSuchElementException` |
| CratonVM `--jdk-only` | `Collections$3` | false | `NoSuchElementException` |
| CratonVM Compatible | **`Enumeration$Impl`** | **true** | **returns** |

**Three divergences at one site, where the brief predicted one.** The class is
fabricated, the fabrication is *additionally* an `Iterator` when the real
carrier deliberately is not, and it does not throw at exhaustion.

## 2. Why nothing caught it — the defect is EMPTY-CONDITIONED

**The populated table was already correct in both arms** (`Hashtable$Enumerator`).
Only the empty case fabricates, because real `Hashtable.getEnumeration(int)`
short-circuits to `Collections.emptyEnumeration()` when `count == 0` rather than
building an `Enumerator` — and that carrier is deliberately **not** an
`Iterator`.

So **a probe that fills the table cannot see this**, and every probe did. This
is the third instance in two days of the standing trap *a narrow probe reports
its own reach, not the defect* — the other two being an `instanceof` probe that
tested interfaces where the defect was in abstract superclasses, and a `HashSet`
probe that tested the wrong population shapes. In all three the probe was
reasonable and the cell was simply not in it.

The premise was already written down: `hashtable_has_entry`'s doc block states
the `count == 0` short-circuit. **The VM had no way to honour it**, because the
empty case fell through to `make_hashtable_enumeration`, whose primary arm
fabricates `java/util/Enumeration$Impl`.

## 3. What changed

A new `real_empty_enumeration` resolves the JDK's own
`Collections.emptyEnumeration()` carrier, returning `None` — **never a
fabrication** — when the method is absent. That absence is the synthetic-JDK
shape, and it is also what `MockNativeContext` and the in-tree test registry
answer, so **both keep the snapshot carrier they have always had** and neither
changes behaviour.

Same rule as the two siblings already in the tree, `real_hashtable_enumerator`
and `classloader::real_snapshot_enumeration`: *prefer a real class the JDK
builds itself over a name no image declares.*

**One deliberate detail worth keeping.** The resolve propagates with `?` rather
than swallowing. The lane's comment gives the reason: the sibling it mirrors is
reached from the same two call sites on the strict arm today and propagates, and
swallowing an `Err(ExceptionThrown)` here would **discard a live `Throwable`**
rather than deliver it. A merely-unexpected shape (a void or null return) still
falls back to the snapshot carrier — which is the case worth being lenient
about. That is the correct split, and it is the one `a-throwing-wrap-loses-the-
bytes-it-produced` was recorded about.

## 4. NOT VERIFIED

* **No arm has run against this change.**
* `RJdkEnumerations` is one of the five standing `SUITE=all` failures and is
  expected to move. Per `H15-1` that is the fix, not a violation — but it is
  **predicted, not measured**, and `RJdkEnumerations` also fails under an armed
  `java/util/Hashtable` dial for a *different* reason (a view carrier with a
  null `this$0`, `H0-5` mechanism A). **Fixing this does not close that**, and
  the two must not be conflated.
* The `NoSuchElementException` half is a second divergence at the same site that
  **no vector reaches**. If it is now correct, nothing in the corpus proves it.

## 5. NOMINATIONS

* **N1 — a vector for the empty case of every container.** This defect existed
  because the corpus only ever populates. `Hashtable`, `Vector`, `Properties`,
  `Collections.empty*` and the `Map`/`Set`/`List` `of()` zero-arg forms all have
  an empty-conditioned path, and none is asked.
* **N2 — exhaustion behaviour is unasked too.** `nextElement()` past the end
  diverged here and no vector calls it. The same question applies to every
  `Iterator`/`Enumeration` the VM hands out.
* **N3 — grep the tree for other `count == 0` / `isEmpty()` short-circuits in
  the real JDK that the VM's stand-in does not model.** This one was documented
  in a doc block the VM could not honour; there is no reason to think it is the
  only one.
