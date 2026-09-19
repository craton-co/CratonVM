# H23-3 — the ratchet's array column counts a fifth of its population, and the defect it hides fails open rather than loudly

**Status:** OPEN. The patch in §2 is **not applied** — `scripts/` is not this
lane's file. All counts MEASURED on the tree at `603f2962f`; all probe output
MEASURED on `cratonvm-r8.exe` vs HotSpot 25.0.3+9, one case per process.

---

## 1. The count

`scripts/untyped-alloc-ratchet.sh`, run on this tree:

```
sites  : 203 (baseline 203)
widths : [1 2 3 4 5 6 7 8 12]
by fn  : alloc_object=173 new_ref_array=29 alloc_object_of=1
ok — no growth, no new widths, no new spellings.
```

Counted directly, over the **same** crate list and the **same** exclusions the
script uses:

```
new_ref_array( <any path> ClassId::new(0), <any second argument> )   152
        ... of which the ratchet's PAT matches                        29     (19%)
```

**Mechanism.** `PAT` ends `, *[1-9][0-9]*\)`: the second argument must be a
literal positive integer. For `alloc_object`/`alloc_object_of` that argument is
a **field count**, and requiring a literal is right — a zero-field carrier is
not a shape worth tracking. For `new_ref_array` the same position is an **array
length**, which is usually a variable and is very often `0`. Measured
distribution across the 152:

```
  42  0             <- excluded by [1-9]
  11  1
   7  2
   6  old_len + 1   <- excluded: not a literal
   6  cap           <- excluded: this was alloc_bucket_table's, the H23-2 defect
   5  elements.len()
   5  3
   4  list.len()
   4  len
   4  items.len()
   3  resp.headers.len()
   3  old_len - 1
```

The largest single group is **length `0`, 42 sites**, and §3 shows a *measured*
defect from exactly that group. `new Object[0]` and `new TypeVariable[0]` are
different classes; a zero-length array still has a component type.

**This is the fifth version of this error and the file's own header documents
the previous four.** `v4` is the near twin — it *"folded `new_ref_array`'s
second argument into WIDTHS, where it is an array LENGTH and not a field
count"*. That fix corrected the `widths` column and left the identical
conflation standing in `sites`, which is the column the gate actually ratchets
on. The note the file quotes at itself, *a gate that measures a FRACTION reads
as good news*, applies to it once more.

**What this does not mean.** The tree did not get worse, and the ratchet is not
useless — it still catches drift in the object-allocator column, which is 174 of
the 203 and where its literal-width logic is correct. What it means is that the
`new_ref_array` column has never been a population count, so **`H0-6`'s
"29 sites tree-wide" should be read as "at least 152"**.

---

## 2. The patch (NOT APPLIED — `scripts/` is not this lane's file)

Give the array allocator its own pattern instead of sharing the object one, and
count it as its own unit. Two hunks:

```diff
 PAT='[A-Za-z0-9_]+\(([A-Za-z0-9_]+::)*ClassId::new\(0\), *[1-9][0-9]*\)'
 OBJPAT='alloc_object(_of)?\(([A-Za-z0-9_]+::)*ClassId::new\(0\), *[1-9][0-9]*\)'
+# The array allocator's second argument is a LENGTH, not a field count: it is
+# usually a variable and is `0` at 42 sites. Requiring a literal positive
+# integer there — which is correct for the OBJECT allocators — hid 123 of 152
+# array sites (81%). See H23-3. Match any second argument for this spelling.
+ARRPAT='new_ref_array\(([A-Za-z0-9_]+::)*ClassId::new\(0\), *[^)]'
```

and, in `hits()` / `breakdown()`, count the union of `PAT` and `ARRPAT` rather
than `PAT` alone. `WIDTHS` must keep using `OBJPAT` only — that is what `v4`
got right and must not be undone.

**Re-baseline required.** `sites` moves 203 → ~326 and `by fn` moves
`new_ref_array=29` → `new_ref_array=152`. That is a *measurement* change, not a
regression, and the commit must say so or the next lane will read it as a
203-site growth. **A ratchet only holds if clearing it is what makes it green** —
so re-baseline in the same commit as the pattern fix.

**Falsifier for this patch, and the one the previous four versions skipped:**
delete a known `new_ref_array(ClassId::new(0), 0)` site and confirm the number
moves by exactly one. `v1` was caught precisely because a lane deleted two sites
and watched the count *not move*. Do that before trusting the new number.

### 2a. That falsifier has now been run against the CURRENT gate, by accident

`H23-2`'s fix removed a sentinel array site from this crate —
`alloc_bucket_table`'s `try_new_ref_array(ClassId::new(0), cap)`, which is the
allocation behind every `HashMap.table` in the VM. Direct count of the array
sentinel in `native-collections/src/lib.rs`: **12 before, 11 after.**

The gate, on the same two trees:

```
before   by fn : alloc_object=173 new_ref_array=29 alloc_object_of=1   ok
after    by fn : alloc_object=173 new_ref_array=29 alloc_object_of=1   ok
```

**Unchanged, and green, rc=0.** The site removed was one of the 123 the gate
cannot see, because its length argument was the variable `cap` rather than a
literal — the sixth row of §1's distribution.

So this is not a projection any more: the flagship defect this gate exists to
watch was fixed, and the gate reported *nothing*. That is the same experiment
that caught `v1`, with the same result, one column over. It is also the reason
§6.1 is the highest-priority nomination in this record — a gate that cannot
register the fix cannot register the regression either.

---

## 3. A measured defect from the excluded group

`Class.getTypeParameters()` must return `TypeVariable[]`; the JDK declares it
`TypeVariable<Class<T>>[]`. `native-builtins/src/lang_class.rs` builds every
return path of it with the sentinel, including four zero-length early returns
(`17162`, `17169`, `17176`, `17183`) and the populated one (`17193`).

MEASURED (`scratchpad/h23/Refl.java`), one case per process:

```
                              HotSpot 25.0.3+9                  CratonVM r8 --jdk-only
TP   (2 type params)   [Ljava.lang.reflect.TypeVariable;   [Ljava.lang.Object;
TP0  (0 type params)   [Ljava.lang.reflect.TypeVariable;   [Ljava.lang.Object;
ABT  getAnnotationsByType      [LRefl$Foo;                 [LRefl$Foo;          ok
IFC  getInterfaces             [Ljava.lang.Class;          [Ljava.lang.Class;   ok
MTH  getDeclaredMethods        [Ljava.lang.reflect.Method; [Ljava.lang.reflect.Method; ok
```

Two things worth separating, because they pull in opposite directions:

* **`TP0` is the zero-length case, and it is wrong.** A concrete instance of the
  42 sites §1 says the gate cannot see. It is not a hypothetical.
* **Three of the five are already correct.** `getInterfaces`,
  `getDeclaredMethods` and `getAnnotationsByType` return properly typed arrays,
  so the 30 sentinel sites in `lang_class.rs` are **not** 30 defects — most of
  that file already routes through a typed builder. Any nomination that quotes
  the raw site count as a defect count repeats §1's own error one level up.
  **Grep before asserting**: the count is a lead, not a quantity.

### 3a. Why nobody noticed: the checkcast fails open

The natural caller is an assignment, and its implicit checkcast is what should
fail:

```java
TypeVariable<?>[] tv = Gen.class.getTypeParameters();   // Object[] -> TypeVariable[]
```

On CratonVM this **succeeds** — `tv.length == 2`, no throw. `array_is_assignable_to`
is documented fail-open on imprecise component information, so an `Object[]`
flows into a `TypeVariable[]` slot unchallenged. So the defect is *silent*: it
is visible to `getClass()`, to `getComponentType()`, and to anything that
reflects on the array, but it does not announce itself.

That is the same shape as the `HashMap.table` half `H23-2` fixed, and it is why
neither showed up in a corpus arm. **A green arm is evidence about the question
it asked**, and nothing in the corpus asks this one.

---

## 4. Tree-wide, where the array sentinel actually lives

All 152, by file (this lane's own crate is now 11, down from 12):

```
  30  native-builtins/src/lang_class.rs          <- H25's file; see §3
  20  native-builtins/src/jmx.rs
  13  native-builtins/src/t27_tls.rs
  11  native-collections/src/lib.rs              <- this lane, H23-2
   7  native-builtins/src/phases_late/beans_jndi.rs
   7  native-builtins/src/jmx_openmbean.rs
   6  native-io/src/lib.rs
   5  native-builtins/src/net_phase_e.rs
   5  native-builtins/src/lang_misc.rs
   4  native-builtins/src/tls.rs
   4  native-builtins/src/servlet.rs
      ... 25 further files at 3 or fewer
```

`jmx.rs` + `jmx_openmbean.rs` = 27 between them and are unowned this round.
`MBeanInfo`/`MBeanAttributeInfo[]`/`ObjectName[]` are strongly-typed in the JMX
API and are read by real `javax.management` bytecode, so that pair is where I
would look next after `lang_class.rs`.

---

## 5. What I did NOT verify

* **The patch in §2 was never run.** The numbers it predicts (203 → ~326,
  29 → 152) are derived from my own direct greps in §1, not from executing the
  modified script. Anyone applying it should expect to adjust the regex and must
  run the delete-one-site falsifier.
* **I did not audit the 152 for correctness.** Six of my own crate's twelve were
  *legitimately* `Object[]` (`H23-2` §2 — `ArrayList` backing stores, `Object[]`
  snapshots, `IdentityHashMap`). The site count is an upper bound on the defect
  count and probably a loose one.
* **I did not check the `jmx.rs` sites at all** beyond counting them.
* **`t27_tls.rs`'s 13 array sites are unexamined.** The ratchet header calls that
  file the single largest producer overall (35 object sites) and notes it is
  production source despite the test-shaped name.

---

## 6. NOMINATIONS

1. **Apply §2 and re-baseline in the same commit**, with the delete-one-site
   falsifier run first. Until then, no lane should quote the `new_ref_array`
   column as a population. **Highest priority in this record** — it is the
   instrument every other array nomination will be measured with.
2. **Restate `H0-6`'s "29 sites tree-wide" as ≥152.** The claim inherits the
   blind spot, not a mistake of its author.
3. **`Class.getTypeParameters()` returns `Object[]`** (§3), all five return
   paths, `lang_class.rs:17162/17169/17176/17183/17193`. `H25`'s file. Small and
   self-contained: the component is `java/lang/reflect/TypeVariable`, and the
   four empty returns need it as much as the populated one.
4. **Audit `jmx.rs` + `jmx_openmbean.rs` (27 sites)** against the JMX API's
   declared array types (§4).
5. **Consider whether `array_is_assignable_to`'s fail-open posture should be
   *observable*** (§3a). It is the right default — it must never invent a
   `ClassCastException` — but it currently makes every component-type defect
   silent. A debug-gated counter of fail-open admissions would have surfaced
   both this and `H23-2`'s defect years earlier, and costs nothing when off.

---

## FIXED (lane H0, 2026-08-21) — v6, and the true population is 359

This record is right, and the fix went further than its diagnosis because the
recount surfaced two more dimensions.

### What the gate now counts

| | v1 | v5 (what this record measured) | **v6** |
|---|---:|---:|---:|
| object sites | 84 | 173 | **208** |
| array sites | 0 | 29 | **151** |
| **total** | **84** | **203** | **359** |

**v5 saw 57% of the population; v1 saw 23%.**

### Two dimensions this record's diagnosis did not name

1. **A fourth allocator spelling.** `try_new_ref_array` — 4 sites — which no
   version of the gate had ever matched.
2. **Two functions take the sentinel and are NOT allocations.** `class_is` (a
   comparison) and `define_class` (where the sentinel means *"no parent"*, not
   *"unknown class"*). A blanket "any function name" pattern — the obvious fix
   after v5 — counts both and **over**-reports, which is the same error in the
   other direction.

So the counting rule is now an explicit **allowlist**, with a per-function rule
for the second argument:

* `alloc_object` / `alloc_object_of` — count unless the width is a **literal
  `0`**, because `vm_exec.rs` substitutes only when `num_fields > 0`. 21 such
  sites exist and fabricate nothing.
* `new_ref_array` / `try_new_ref_array` — count **regardless of length**,
  including a literal `0` and, decisively, a **variable**. This record's
  finding: an array length is naturally dynamic, so a literal-only pattern is
  blind to most of the population *by construction*.

**Unknown spellings are discovered but not counted.** The breakdown greps every
function that takes the sentinel, so a new one appears as a named line and trips
a new-spelling check that asks a human to classify it. That is the fail-safe
direction — v1 and v2 were wrong precisely because a spelling they had never
seen was silently *absent* rather than loudly *unclassified*.

### The falsifier this record ran by accident is now the acceptance test

`H23` removed the sentinel behind every `HashMap.table` and the gate still
reported **29 → 29, green**. A `--selftest` mode now asserts both patterns match
something, and the three failure paths are exercised and confirmed: array growth
trips, object improvement reports, a new function spelling trips.

### Six versions, and the shape of the error never changed

Each version measured a fraction and printed a confident number: 41%, then a
regex that matched nothing, then rc=0 while matching nothing at all, then three
invented carrier families, then 19% of the array column. **The failure mode of a
census is not a wrong answer — it is a confident partial one**, and it survived
five rounds of a person who had written that sentence into the file's own header.

The general lesson is worth more than the gate: **a census needs a falsifier it
runs itself.** Every one of these was caught by an outsider deleting a site and
watching the number fail to move. That test cost nothing and no version of the
gate performed it on itself until now.
