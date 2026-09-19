# H0-6 — mechanism B has one producer, and the surface it feeds grew 71% in eight days

**Status: OPEN — MEASURED.** Counts are `git grep` over the tree at `fe59bf9d9`
and at `684707e60`. No source change, no build.

Lane H0 (orchestrator), 2026-08-20. Answers `H0-5` §7 N2.

---

## 1. The producer, which is a single substitution

`H0-5` §4 found four vectors dying on a fabricated receiver:

```text
java/lang/ArrayStoreException: cratonvm.synthetic.AnonymousObject$4
java/lang/ClassCastException: class cratonvm.synthetic.AnonymousObject$4 cannot be cast to ...
```

and nominated "one grep should name the producer". It does. There is exactly
one, at `vm/src/vm/vm_exec.rs:12894`:

```rust
let class_id = if class_id == ClassId::new(0) && num_fields > 0 {
```

**`AnonymousObject$N` is not allocated by anyone. It is substituted.** When a
native calls `alloc_object(ClassId::new(0), N)` — the untyped-allocation
sentinel — the VM replaces the sentinel with
`cratonvm/synthetic/AnonymousObject$N`, a class declaring exactly `N` fields, so
that the object header's `class_id` agrees with its slot count.

The comment above it states the consequence better than I would:

> *"The caller, meanwhile, resolved a class and FAILED, then handed the object
> out as an instance of the class it named."*

So mechanism B is not four bugs and not a collection bug. It is **one sentinel,
consumed by every caller that could not resolve the class it wanted**, and the
four vectors in `H0-5` §4 are four consumers of the same substitution.

## 2. Why no census has ever counted it

Also from that comment, and this is the part worth carrying:

> *"every one of these allocations arrives at the census downstream as
> `classify(n, n)` — perfect agreement — and prints nothing in either
> direction. … both directions of the census were structurally blind to all of
> them: `under` cannot fire because the clamp runs first, and this path cannot
> fire because the substitution makes the widths agree."*

**The substitution makes the object self-consistent, which is exactly what
defeats a width census.** A fabricated 4-field carrier handed out as a
`Map.Entry` is wrong in every way that matters and wrong in no way the
instrument was built to see. This is the same species as `H1-1`'s capped sink
and `H0-4`'s fraction gate: *the instrument reported health because of how the
defect is shaped, not because the defect was absent.* Third instance in this
directory in two days.

The `layout_alias` observation flag was added on 2026-08-12 (`684707e60`)
specifically to see past it, reporting `declared = 0` under
`layout_alias::UNRESOLVED_CLASS` rather than the substitute's name — the honest
statement being *"no declared layout was available here"*.

## 3. MEASURED — the surface grew 71% in eight days

Production sites in the native crates spelling the sentinel with a literal
width, excluding paths matching `test`:

```
git grep -n "alloc_object(ClassId::new(0), *[1-9][0-9]*)" <rev> -- \
    native-builtins/src native-collections/src native-io/src native-api/src \
    native-builtins-crypto/src native-builtins-security/src native-awt/src \
  | grep -v test | wc -l
```

| revision | date | sites |
|---|---|---:|
| `684707e60` | 2026-08-12 | **49** |
| `fe59bf9d9` | 2026-08-20 | **84** |

**+35 sites, +71%, in eight days.** Widths today: `1`×34, `2`×17, `4`×13,
`5`×12, `3`×3, `8`×3, `6`×1, `12`×1.

Concentrated, and not where the collection work is looking:

| file | sites |
|---|---:|
| `native-builtins/src/util_concurrent_ext.rs` | 24 |
| `native-builtins/src/lang_class.rs` | 14 |
| `native-builtins/src/reference.rs` | 7 |
| `native-builtins/src/lib.rs` | 7 |
| `native-builtins/src/net_phase_e.rs` | 5 |
| `native-collections/src/lib.rs` | **4** |

`native-collections` — the crate the whole migration plan is organised around —
holds **four of eighty-four**.

### 3a. What I am NOT claiming

The comment at `684707e60` says **"30 production sites"**; my method counts
**49** at that same commit. **I do not know what the difference is** — the
comment may count distinct classes, or resolvable-class sites only, or a
narrower crate set. Two honest consequences:

* **The `30` and my `84` are not comparable**, and quoting "30 → 84" would be
  the arithmetic this directory keeps recording as false.
* **The `49` → `84` comparison is sound**, because it is one method applied to
  two revisions. That is the growth claim, and it is the only one I make.

I checked this specifically because asserting the comment was wrong was the
cheaper and more satisfying move. It would also have been unverified.

## 4. Why the growth matters more than the level

The plan of record is to migrate the fabricated-carrier producers to **real
construction**. That plan is written against a fixed target. It is not fixed:
**the surface is being added to faster than any wave has removed from it**, by
lanes that are not doing collection work and are not reading this directory.

`native-builtins/src/util_concurrent_ext.rs` alone holds 24 — more than a
quarter of the whole surface, in the file that also defines
`try_alloc_concurrent_synthetic`. Nothing has proposed touching it.

**A migration with no ratchet loses.** `H3-1` repaired the stub ratchet for
exactly this reason and it counts registrations, not allocation sites: the
sentinel is invisible to it, because a substitution is not a registration.

## 5. NOMINATIONS

* **N1 — a ratchet on the sentinel, two columns.** Count of
  `alloc_object(ClassId::new(0), N)` production sites, and count of distinct
  widths. It is one `git grep` and it would have caught this on 2026-08-13.
  Cheap enough that not having it is the finding. *(Lane H10 owns the gate
  files this round; this is a nomination to it, not an edit by me.)*
* **N2 — re-derive the "16 name a class WIDER than N" subset.** The comment
  names `ZipEntry` 6 against 14, `Pattern` 2 against 20, `ServiceLoader` 2
  against 10, and `java/lang/Thread` 5 against 19 **at two sites that are not
  even fallback arms**. That subset is the actively harmful half and it was
  measured against a 49-site population, so **the number is stale by
  construction**. Recomputing it needs each site's intended class resolved
  against real JDK 25 layouts. **NOT DONE HERE, and I am not estimating it.**
* **N3 — `util_concurrent_ext.rs` is the largest single producer and is
  unclaimed by any lane or any P0 row.** 24 of 84. Before that file is
  migrated, someone should say why 24 untyped allocations are correct there.
* **N4 — does `--jdk-only` refuse the substitution?** `class_manager.rs:18089`
  carries the note *"Compatible mode fabricates; this fixture never runs under
  `--jdk-only`"*, which implies strict mode does not fabricate. If so, every one
  of the 84 sites is a **strict-mode failure path**, not a wrong-answer path,
  and the two want different repairs. **ARGUED from one comment; not measured.**

---

## 6. MEASURED, same day — N4 is answered, and against my own guess

**Strict mode does not refuse the substitution.** `vm_exec.rs:12997`:

> *"`ensure_generated_class`, not `ensure_synthetic_class`: this is a VM
> bookkeeping type, not a compatibility substitution. … contract §1 item 6 makes
> it legitimate in **both** modes."*

Stamping it `CompatibilityStub` was tried and reverted, because it made contract
§11's zero-stub criterion unachievable by construction — a strict boot reported a
`compatibility-class-requested` violation for a class the contract permits, so
*"the census could never reach zero however much real work was done."*

So my N4 guess was wrong, and the correction matters: **the 84 sites are
wrong-answer paths in both modes**, not strict-mode failure paths. They do not
show up as a refusal anywhere. `compatibility_classes, SUM: 0` in the census is
true and tells you nothing about them.

## 7. MEASURED — what `AnonymousObject$4` actually is

There is a live flag nobody in this directory has used: **`CRATONVM_DBG_ANONALLOC=1`**,
which forces the slow path and dumps a 12-frame stack at every substitution.

```
CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HashMap CRATONVM_DBG_ANONALLOC=1 \
  cratonvm.exe --jdk-only -cp build RJdkCollections
```

**2204 substitutions in one vector.** Widths: `4`×2172, `3`×32. Nothing else.

And the failure it ends in:

```text
Exception in thread "main" java/lang/ArrayStoreException: cratonvm.synthetic.AnonymousObject$4
	at RJdkCollections.maps(RJdkCollections.java:122)
	at java/util/HashMap.merge(HashMap.java:1372)
	at java/util/HashMap.resize(HashMap.java:719)
```

**`AnonymousObject$4` is the `HashMap.Node`.** Width 4 is
`{hash, key, value, next}`. Real JDK `HashMap.resize()` executes
`newTab[i] = e` into a `Node[]`, and what it holds is the VM's untyped carrier.
The array store checks the class and refuses it.

### This unifies `H0-5`'s two mechanisms into one root

`H0-5` §3 and §4 reported mechanism A (view carrier, null `this$0`) and
mechanism B (fabricated carrier reaching an array store) as two defects wanting
different fixes. **They are one defect seen from two sides:**

| | what the VM owns | how real bytecode trips on it |
|---|---|---|
| **A** | the *view* object | reads `this$0.modCount` — null |
| **B** | the *node* object | stores it into `Node[]` — wrong class |

The root is that `java.util.HashMap`'s **internal representation** is the VM's,
not the class file's. The repair is one repair — real construction — and it is
the same one `H4-1` O2 named. `H0-5` should be read with this correction; I am
recording it here rather than editing that record's conclusion, because the
measurement that overturned it is this one.

## 8. The instrument attributes 4.4%, and it is not the JIT

Of 2204 substitutions, **97 printed a stack. 2107 printed nothing** — a blank
line where twelve frames should be, because `self.thread.frames` was empty.

The obvious hypothesis is that the missing 96% happen in JIT-compiled code,
which pushes no interpreter frames. **Tested and DISPROVED:** the same vector
with `--nojit` gives **2204 events and 97 attributed — identical, to the event.**
Whatever empties the frame stack at these allocations, it is not tier-up. I did
not find the cause and am not guessing at it.

What the 97 do say is worth having:

| attributed frame | count |
|---|---:|
| `jdk/internal/module/ServicesCatalog.register` / `.addProviders` | 41 |
| `RJdkCollections.maps()` | 24 |
| `ServicesCatalog.<init>` via `BuiltinClassLoader.<clinit>` | 16 |

**The JDK's own service registry is a consumer.** `ServicesCatalog` is
`ConcurrentHashMap`-backed, which is the connection to `RServiceLoaderDoubleSource`
(one of the five standing `SUITE=all` failures) and to `RJdkModule` and
`RJdkProxyIface` (two of `H0-3`'s eleven CHM casualties). Those three have been
filed as separate module/proxy/service-loading problems. **They may be this one.**
*Not proven — the frames are from the armed run, and the standing failures are
unarmed. Stated as a lead, and handed to the lanes holding those vectors.*

## 9. NOMINATIONS, revised

* **N5 — fix the 96% attribution gap before anyone plans from this instrument.**
  A dump that silently drops 2107 of 2204 events is the same species as `H1-1`'s
  capped sink: it answers, it looks complete, and it is a 4% sample. The JIT is
  ruled out; the cause is not known.
* **N6 — `AnonymousObject$4` is one shape with one meaning, so the ratchet in N1
  can be sharper than I proposed**: alert on any NEW width, and on any growth in
  width-4 sites, separately.
* **N2 is now more urgent, not less.** The "16 name a class WIDER than `N`"
  subset was measured against 49 sites and there are 84. Still **NOT DONE**.

---

## 10. CORRECTION (lane H0, 2026-08-21) — §3's 84 was 41% of the surface, and §4's top producer was the wrong file

**Lane `H16` caught this by deleting two sentinel sites and watching the ratchet
not move.** The method in §3 — and the blocking gate built from it in
`scripts/untyped-alloc-ratchet.sh` — matched only the **bare** `ClassId::new(0)`
spelling. The majority spelling in this tree is fully qualified,
`cratonvm_types::ClassId::new(0)`, and there are two other allocator functions
taking the same sentinel.

**Measured over the same crate scope, same revision, all spellings:**

| allocator | sites | what it fabricates |
|---|---:|---|
| `alloc_object` | 173 | an object whose CLASS is unknown → `AnonymousObject$N` |
| `new_ref_array` | 29 | a reference ARRAY whose COMPONENT class is unknown → `Object[]` |
| `alloc_object_of` | 1 | as `alloc_object` |
| **total** | **203** | |

**§3's `84` is 41% of `203`.** Every conclusion in §3 and §4 that rests on the
absolute number is therefore understated, and the ratchet built from it was
reporting a clean `ok` over 41% of its own subject.

### §4's named top producer is wrong

§4 says *"`util_concurrent_ext.rs` alone holds 24 — more than a quarter of the
whole surface."* Measured with every spelling:

| file | sites |
|---|---:|
| `native-builtins/src/t27_tls.rs` | **34** |
| `native-builtins/src/util_concurrent_ext.rs` | 29 |
| `native-builtins/src/lang_class.rs` | 15 |
| `native-builtins/src/http_url_connection.rs` | 12 |
| `native-collections/src/lib.rs` | 9 |

**`t27_tls.rs` is the largest producer and appears in no record anywhere.** It is
production source in `native-builtins/src/` despite the test-shaped name, which
is very likely why it was never noticed — and it is the kind of file a
`grep -v test` filter deletes silently. (This gate excludes test paths by
pathspec precisely so that it cannot.)

### The growth claim in §3 survives; the level does not

§3's `49 → 84` was one method applied to two revisions, so **the growth is still
sound as a growth**. What is not sound is reading `84` as the size of the
surface. It is 203, and the honest statement is that **nobody has ever measured
the level correctly until now**.

### `new_ref_array` is not a footnote

29 sites allocate a reference array with an **unknown component class**, i.e. an
`Object[]` where the real class declares a typed array. That is the other half
of `H0-6` §7's `ArrayStoreException`: `HashMap.table` is
`[Ljava/lang/Object;` where JDK 25 declares `[Ljava/util/HashMap$Node;`. `H16`
independently reports the same thing from the other end and nominates it as
unfixed. **The node class and the array component type are two defects, and
fixing only the first leaves the store failing.**

### What the gate now does about it

`scripts/untyped-alloc-ratchet.sh` was rewritten: extended-regex matching over
**all three allocator spellings**, a **per-function breakdown** so a fourth
spelling appears as a named line rather than hiding in a total, `WIDTHS` taken
from the object allocators only (`new_ref_array`'s second argument is a LENGTH,
not a field count — folding them made 16/32/64 read as three new carrier
families), and a **zero-guard that exits 3**, because one revision of it printed
`ok — no growth` with `rc=0` while matching nothing at all.

Four successive versions of this gate each printed a confident wrong number.
That is worth recording as its own finding: **the failure mode of a census is
not usually a wrong answer, it is a confident partial one.**
