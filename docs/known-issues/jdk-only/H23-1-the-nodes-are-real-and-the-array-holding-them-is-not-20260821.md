# H23-1 — the nodes are real, the array holding them is not, and the gate that watches this sees 19% of it

**Status:** OPEN. Measurement complete; the fix is `H23-2`. Every number below is
MEASURED on the prebuilt `C:/craton/cratonvm-r8.exe` (clean build at `025780ff7`)
against oracle HotSpot 25.0.3+9-LTS, unless marked ARGUED.

---

## 1. What was in the worktree gap

This lane's worktree was cut at `26e4b5db4` (a `dev` merge), not at the branch
tip `19a492a4a`. The gap was **132 commits, 126 files, +27,577 / −1,391 lines** —
the entire `H0`–`H22` body of work, *including `H16`'s node-class fix in this
lane's own file* (`native-collections/src/lib.rs`). Editing without
`git merge --ff-only claude/jdk-only-mode-handoff-09b48c` first would have
reverted it. Twelve of twelve lanes have now hit this.

---

## 2. The probe, and why it is one case per process

`regression-suite` asks nothing about component types, so **a green arm is not
evidence here** — the question has to be asked directly. The probe reads
`HashMap.table` reflectively under
`--add-opens java.base/java.util=ALL-UNNAMED` and prints the array class, the
component class, and the class of each occupied bucket.

`H0-8` measured that a probe whose cases run in sequence is a repeated-measures
design confounded with anything latched per process — it manufactured four false
"discriminators" that were pure case order. So the probe takes the case as
`argv[0]` and **each case is a separate JVM process.** Five cases:

| case | workload |
|---|---|
| A | `new HashMap<>()`, never put |
| B | three `String`→`String` puts |
| C | 40 puts — forces `resize()` |
| D | `put("k", 7)` — autoboxed `Integer` value |
| E | `put("k", null)` — null value |

## 3. The measurement

```
            HotSpot 25.0.3+9                      CratonVM r8 --jdk-only
case A   tableCls=null              size=0     tableCls=[Ljava.lang.Object;  len=16 size=0
case B   [Ljava.util.HashMap$Node;  len=16     [Ljava.lang.Object;           len=16 size=3
case C   [Ljava.util.HashMap$Node;  len=64     [Ljava.lang.Object;           len=64 size=40
case D   [Ljava.util.HashMap$Node;  len=16     [Ljava.lang.Object;           len=16 size=1
case E   [Ljava.util.HashMap$Node;  len=16     [Ljava.lang.Object;           len=16 size=1
```

Bucket contents, **both VMs, cases B–E**: `java.util.HashMap$Node`. `H16`'s node
fix holds on this binary in every case measured, including the two that were
candidates to fall back (`D` boxed value, `E` null value).

**So the two halves are exactly out of step:** the nodes are the real class, the
array holding them is `Object[]`, and `table.getClass()` is wrong on every
non-empty map.

### 3a. A second, independent divergence in case A

HotSpot's `HashMap.table` is **null until the first put** — the table is
allocated lazily in `resize()`, not in the constructor. CratonVM allocates a
16-slot table eagerly in the constructor (`alloc_bucket_table`, and its doc
comment already knows this: *"HotSpot allocates the table lazily on the first
insert"*). Two consequences:

* it is a real, separately observable `--jdk-only` divergence — `table == null`
  is the documented "not yet initialised" signal that `HashMap`'s own bytecode
  branches on; and
* **any probe that reads `table` before a put measures nothing on HotSpot.** A
  probe built without a put would have compared `null` against `Object[]` and
  read as "CratonVM has a table and HotSpot does not", which is not the defect.

Not fixed here — eager allocation is load-bearing for the native map's
`__capacity`/`threshold` bookkeeping and changing it is not this lane's file
alone. Nominated in §7.

---

## 4. The sentinel population in this file

`new_ref_array(ClassId::new(0), n)` is the array half of the untyped-allocation
sentinel: the component class is unknown, so the array becomes `Object[]`.
**Twelve sites in `native-collections/src/lib.rs`**, with the component class the
caller meant:

| line | site | component the caller meant |
|---|---|---|
| 3155 | `alloc_ref_array` (helper, **161 callers**) | *varies* — genuinely untyped, the general helper |
| 3162 | `alloc_ref_array_or_oom` (helper, 3 callers) | *varies* |
| 3333 | `alloc_bucket_table` | **`java/util/HashMap$Node`** ← the defect in §3 |
| 5241 | `ArrayList` backing store | `java/lang/Object` — **correct as-is** |
| 40186 | `ArrayDeque`/queue backing store | `java/lang/Object` — **correct as-is** |
| 41120 | growable buffer | `java/lang/Object` — **correct as-is** |
| 68216 | segment bucket table | `java/util/concurrent/ConcurrentHashMap$Node` |
| 68252 | segment bucket table | `java/util/concurrent/ConcurrentHashMap$Node` |
| 68278 | segment bucket table (cap 32) | `java/util/concurrent/ConcurrentHashMap$Node` |
| 68665 | entry/element snapshot | `java/lang/Object` — **correct as-is** |
| 68736 | entry/element snapshot | `java/lang/Object` — **correct as-is** |
| 68787 | 2-element pair | `java/lang/Object` — **correct as-is** |

Six of the twelve are genuinely `Object[]` and are **not** defects: a
`List`/`Deque` backing store and an `Object[]` snapshot are `Object[]` on HotSpot
too. The gate cannot tell those apart from the four that are wrong — which is
the point of §5.

---

## 5. The ratchet sees 29 of 152 array sites — and the reason is the same one it has been wrong four times before

`scripts/untyped-alloc-ratchet.sh` reports, on this tree:

```
sites  : 203 (baseline 203)
widths : [1 2 3 4 5 6 7 8 12]
by fn  : alloc_object=173 new_ref_array=29 alloc_object_of=1
ok — no growth, no new widths, no new spellings.
```

That `new_ref_array=29` is **19% of the population.** Counted directly over the
same crate set and the same exclusions:

```
new_ref_array(<any path>ClassId::new(0), <any second arg>)   152 sites
        ... of which the ratchet's pattern matches                29 sites
```

**Mechanism.** `PAT` ends `, *[1-9][0-9]*\)` — the second argument must be a
*literal positive integer*. That is right for the OBJECT allocators, where the
second argument is a **field count** and a zero-field carrier is not a shape
worth tracking. It is wrong for `new_ref_array`, where the second argument is an
**array length**, which is usually a variable. Measured distribution of that
second argument across the 152:

```
  42  0            <- excluded by [1-9]; a zero-length array still has a component type
  11  1
   7  2
   6  old_len + 1  <- excluded: not a literal
   6  cap          <- excluded: this is alloc_bucket_table's, the defect in §3
   5  elements.len()
   5  3
   4  list.len()
   4  len
   4  items.len()
```

The single largest group is **length `0` — 42 sites** — and `new Object[0]` and
`new Node[0]` are different classes, so those are real instances of the defect,
excluded by construction.

**This is the fifth version of the same error, and the file's own header
documents the previous four.** `v4` is the closest twin: it *"folded
`new_ref_array`'s second argument into WIDTHS, where it is an array LENGTH and
not a field count"*. That fix corrected the `widths` column and left the
identical conflation standing in the `sites` pattern, which is the column the
ratchet actually gates on. The repository note this file quotes at itself —
*a gate that measures a FRACTION reads as good news* — applies to it again.

Note the direction: this does **not** mean the tree got worse. It means the
`new_ref_array` column has never been a population count, and `H0-6`'s
"29 sites tree-wide" should be read as **at least 152**.

`scripts/` is not this lane's file. The patch is written out in `H23-3` §1
rather than applied.

---

## 6. Why typing the array is not a free rename — the store IS checked

The half-fix hazard is real and this is where it lives.

* **Interpreter `aastore` enforces the component type.**
  `vm/src/runtime/interpreter/opcodes.rs` runs the JVMS §6.5 covariance check
  (NPE → AIOOBE → ArrayStoreException) via
  `typecheck::aastore_element_assignable`. The JIT's `jit_aastore` shares the
  same predicate.
* **The native path does not.** `NativeContext::set_array_element`
  (`vm/src/vm/vm_exec.rs:12268`) writes straight through
  `heap.set_array_element` with a bounds check and **no covariance check at
  all.** Every store this crate makes into a bucket table takes that path.
* **A fabricated node would not be rescued by the fail-open hatches.** ARGUED
  from source: `aastore_element_assignable` fails open on synthetic ids
  `>= 0x8000_0000`, on `$Proxy` names, and on classes absent from the class
  store. A fabricated `cratonvm/synthetic/AnonymousObject$N` is registered by
  `ensure_generated_class` (`vm/src/vm/vm_exec.rs:13026`), which mints an
  ordinary **dense** id and a real class-store entry. So it misses every hatch,
  its superclass walk reaches only `java/lang/Object`, and the store into a
  `[Ljava/util/HashMap$Node;` **throws.**

So the asymmetry is: **typing the table is invisible to this crate's own writes
and armed for real JDK bytecode's** — and real `HashMap.resize()` storing into
`table` is exactly the `HashMap.java:719` site `H0-6` §7 measured.

That is the whole reason node-class-first was the safe order, and it is the
invariant `H23-2` has to preserve:

> **A table may be typed only if every node that can enter it is real.**

The tree already contains one correct implementation of that invariant —
`chm_publish_real_table_pinned` builds a `try_new_ref_array(node_cid, cap)` and
**abandons the whole publish** (`return None`, caller keeps the untyped carrier)
the moment any entry has a primitive key or value. All-or-nothing, and the pair
never gets out of step.

### 6a. How big is the population that would break it?

Smaller than it looks, and this is measured, not assumed.
`map_node_class_for` downgrades to `ClassId::new(0)` when
`!matches!(key, Value::Object(_)) || !matches!(value, Value::Object(_))`.
`Value::Object(None)` — a Java `null` — **matches** `Value::Object(_)`, so a null
key or value keeps the real node. Case E confirms it on the binary.

Cases B/C/D/E together say: **every put reachable from Java bytecode produced a
real node**, including the boxed-primitive and null shapes that were the
candidates to fall back. The fabricated-node population is the *Rust-private*
one named in `map_node_class_for`'s own doc — `present_marker`'s legacy `Int(1)`
for a null set element, and `materialize_hm_int_fast` re-putting a side-stored
`Value` verbatim through the 42 direct Rust call sites that are "not obliged to
store an object".

**NOT VERIFIED:** I did not witness a fabricated node in a live table on this
binary. I did not find a Java-reachable workload that produces one. That the
population is Rust-private is ARGUED from `map_node_class_for`'s source plus
four negative cases — it is not a proof that no Java path reaches it, and a
`HashSet` holding a null element is the shape I would attack first.

---

## 7. NOMINATIONS

1. **`scripts/untyped-alloc-ratchet.sh` undercounts `new_ref_array` by 5×.**
   Split the pattern so the array allocator admits a non-literal and a zero
   length. Patch in `H23-3` §1. Not this lane's file. **This should land before
   any lane quotes the array column again.**
2. **`H0-6`'s "29 sites tree-wide" needs restating as ≥152.** The number came
   from the gate in §5 and inherits its blind spot.
3. **`HashMap.table` is eagerly allocated where HotSpot allocates lazily**
   (§3a). `table == null` is a state `HashMap`'s own bytecode branches on, and
   CratonVM never presents it. Owner: whoever owns the native map constructor —
   it is `native-collections`, but the `__capacity`/`threshold` coupling makes it
   a separate change from `H23-2`.
4. **The four largest non-collection producers are unexamined**:
   `native-builtins/src/lang_class.rs` (30 array sites), `jmx.rs` (20),
   `t27_tls.rs` (13), `phases_late/beans_jndi.rs` (7). `lang_class.rs` is `H25`'s
   file; the others are unowned this round. A `Class[]`/`Method[]`/`Field[]`
   returned as `Object[]` is the same defect as §3 in the reflection surface,
   and `getInterfaces()`/`getDeclaredMethods()` are far more likely to be
   *observed* than `HashMap.table` is.
5. **`aastore_element_assignable` fails open on a fabricated carrier only by
   accident.** It misses the `>= 0x8000_0000` hatch because
   `ensure_generated_class` mints dense ids. If any lane types an array whose
   contents can still be fabricated, this is the site that turns it into a
   thrown exception. Worth an explicit named arm rather than leaving it to the
   dense-id coincidence — in either direction, deliberately chosen.
