# H18-3 — the `RandomAccess` row costs 886x, measured; and the tier that did not get the fix is the tier that runs the loop

**Status: MEASURED (§2). OPEN (§3 — the three tier twins, patch written out,
not applied: `vm/src/jit/**` and `vm/src/vm/vm_exec.rs` are outside this lane's
ownership).**

**Date** 2026-08-21
**Lane** H18
**Companions** `H18-1` (the fix), `H18-2` (the message)
**Binary** `C:/craton/cratonvm-r5.exe` @ `9eef86699` — prebuilt, pre-patch
**Oracle** HotSpot 25.0.3+9

Claims are **MEASURED** (ran it) or **ARGUED** (read it).

---

## 1. The probe

Not filed under `regression-suite/probes/` — outside this lane's ownership.
Printed here so the next lane can paste it. Compile with the oracle's `javac`;
run on all three arms. Section D is the new part.

```java
import java.util.*;

public class H18Probe {
    static String io(Object o, Class<?> t) {
        boolean opcode;
        if (t == AbstractMap.class)             opcode = o instanceof AbstractMap;
        else if (t == AbstractCollection.class) opcode = o instanceof AbstractCollection;
        else if (t == AbstractList.class)       opcode = o instanceof AbstractList;
        else if (t == AbstractSet.class)        opcode = o instanceof AbstractSet;
        else if (t == RandomAccess.class)       opcode = o instanceof RandomAccess;
        else if (t == SortedSet.class)          opcode = o instanceof SortedSet;
        else if (t == NavigableSet.class)       opcode = o instanceof NavigableSet;
        else if (t == Set.class)                opcode = o instanceof Set;
        else if (t == Collection.class)         opcode = o instanceof Collection;
        else throw new IllegalArgumentException("add an arm for " + t);
        boolean refl = t.isInstance(o);
        return (opcode ? "T" : "F") + "/" + (refl ? "T" : "F") + (opcode != refl ? "*" : " ");
    }
    static void rowA(String tag, Object o) {
        System.out.printf("A %-30s AbsMap=%s AbsColl=%s RandAcc=%s  getClass=%s%n",
            tag, io(o, AbstractMap.class), io(o, AbstractCollection.class),
            io(o, RandomAccess.class), o.getClass().getName());
    }
    static String cast(Object o, int which) {
        try {
            switch (which) {
                case 0: { AbstractMap<?,?> m = (AbstractMap<?,?>) o;  return "cast-ok:" + (m != null); }
                case 1: { AbstractCollection<?> c = (AbstractCollection<?>) o; return "cast-ok:" + (c != null); }
                default:{ RandomAccess r = (RandomAccess) o;          return "cast-ok:" + (r != null); }
            }
        } catch (ClassCastException e) { return "CCE: " + e.getMessage(); }
    }
    public static void main(String[] a) {
        System.out.println("== A: instanceof/isInstance ('*' = the two doors disagree) ==");
        rowA("Map.of()", Map.of());
        rowA("Map.of(k,v)", Map.of("k","v"));
        rowA("Map.copyOf", Map.copyOf(new HashMap<>(Map.of("k","v"))));
        rowA("List.of()", List.of());
        rowA("List.of(1,2,3)", List.of(1,2,3));
        rowA("Set.of(x)", Set.of("x"));
        rowA("Collections.unmodifiableList", Collections.unmodifiableList(new ArrayList<>(List.of(1))));

        System.out.println("\n== B: CHECKCAST -- the verdict, and what the VM's message calls the receiver ==");
        System.out.println("B Map.of(k,v)    -> AbstractMap        : " + cast(Map.of("k","v"), 0));
        System.out.println("B List.of(1,2,3) -> AbstractCollection : " + cast(List.of(1,2,3), 1));
        System.out.println("B List.of(1,2,3) -> RandomAccess       : " + cast(List.of(1,2,3), 2));
        List<Integer> ul = Collections.unmodifiableList(new ArrayList<>(List.of(1)));
        System.out.println("B unmodList      -> RandomAccess       : " + cast(ul, 2));
        System.out.println("B NEGCTRL unmodList -> AbstractCollection (CCE on BOTH) : " + cast(ul, 1));
        System.out.println("B NEGCTRL HashMap   -> AbstractMap     (ok  on BOTH) : " + cast(new HashMap<>(), 0));

        System.out.println("\n== C: the families H0-2 N3 says were never probed ==");
        SortedSet<String> ss = Collections.unmodifiableSortedSet(new TreeSet<>(Set.of("a","b")));
        NavigableSet<String> ns = Collections.unmodifiableNavigableSet(new TreeSet<>(Set.of("a","b")));
        Collection<String> uc = Collections.unmodifiableCollection(new ArrayList<>(List.of("a")));
        Set<String> ks = Collections.unmodifiableMap(new HashMap<>(Map.of("k","v"))).keySet();
        Set<?>     es = Collections.unmodifiableMap(new HashMap<>(Map.of("k","v"))).entrySet();
        System.out.printf("C sortedSet    AbsColl=%s AbsSet=%s Sorted=%s Nav=%s getClass=%s%n",
            io(ss,AbstractCollection.class), io(ss,AbstractSet.class), io(ss,SortedSet.class),
            io(ss,NavigableSet.class), ss.getClass().getName());
        System.out.printf("C navSet       AbsColl=%s AbsSet=%s Sorted=%s Nav=%s getClass=%s%n",
            io(ns,AbstractCollection.class), io(ns,AbstractSet.class), io(ns,SortedSet.class),
            io(ns,NavigableSet.class), ns.getClass().getName());
        System.out.printf("C collection   AbsColl=%s Coll=%s getClass=%s%n",
            io(uc,AbstractCollection.class), io(uc,Collection.class), uc.getClass().getName());
        System.out.printf("C keySet       AbsColl=%s AbsSet=%s Set=%s getClass=%s%n",
            io(ks,AbstractCollection.class), io(ks,AbstractSet.class), io(ks,Set.class), ks.getClass().getName());
        System.out.printf("C entrySet     AbsColl=%s AbsSet=%s Set=%s getClass=%s%n",
            io(es,AbstractCollection.class), io(es,AbstractSet.class), io(es,Set.class), es.getClass().getName());

        System.out.println("\n== D: the price of the RandomAccess row, A/B WITHIN one VM ==");
        int n = 60000, reps = 40;
        List<Integer> plain = new ArrayList<>(n);
        for (int i = 0; i < n; i++) plain.add(i * 2);
        List<Integer> wrapped = Collections.unmodifiableList(plain);
        System.out.println("D wrapped instanceof RandomAccess = " + (wrapped instanceof RandomAccess));
        long sink = 0;
        for (int w = 0; w < 3; w++) { sink += bs(plain, n, reps); sink += bs(wrapped, n, reps); }
        long tPlain = 0, tWrap = 0;
        for (int w = 0; w < 5; w++) {
            long t0 = System.nanoTime(); sink += bs(plain, n, reps);   tPlain += System.nanoTime() - t0;
            long t1 = System.nanoTime(); sink += bs(wrapped, n, reps); tWrap  += System.nanoTime() - t1;
        }
        System.out.printf("D binarySearch plain=%d ms  wrapped=%d ms  ratio=%.1fx  (sink=%d)%n",
            tPlain/1_000_000, tWrap/1_000_000, (double) tWrap / (double) tPlain, sink);
    }
    static long bs(List<Integer> l, int n, int reps) {
        long acc = 0;
        for (int r = 0; r < reps; r++) acc += Collections.binarySearch(l, (r * 7919) % (2 * n));
        return acc;
    }
}
```

## 2. The price, MEASURED — and why this instrument works on a shared host

`H0-2` and `H15-2` both call the `RandomAccess` row *"the one that costs
something"* and both leave the cost **ARGUED**: `Collections.binarySearch`,
`reverse`, `shuffle`, `fill`, `copy` and `swap` branch on
`list instanceof RandomAccess` and fall back to a `ListIterator` walk. Neither
priced it, because the answers stay correct and no value-diffing vector can see
a slow-but-right result.

The instrument is an **A/B inside one VM, one run**: an `ArrayList` against
`Collections.unmodifiableList` **of that same ArrayList**. Same data, same
process, same JIT state, same host load. Every source of noise this project has
written down — shared-host wall time, load-dependent PASS/FAIL, background
agents — cancels in the **ratio**. It needs no HotSpot column to be readable,
and no wall-clock threshold to be a gate.

**MEASURED**, `n=60000`, 40 searches per call, 3 warm-up pairs, 5 timed pairs:

| arm | `wrapped instanceof RandomAccess` | plain | wrapped | **ratio** |
|---|---|---|---|---|
| CratonVM Compatible, run 1 | **false** | 37 ms | 33 585 ms | **885.7x** |
| CratonVM Compatible, run 2 | **false** | 34 ms | 31 420 ms | **911.4x** |
| CratonVM `--jdk-only` | true | 2 ms | 3 ms | **1.0x** |
| HotSpot 25.0.3+9 | true | <1 ms | <1 ms | **1.0x** |

**Three orders of magnitude, reproduced.** Thirty-one seconds against
thirty-four milliseconds, for the same 200 binary searches over the same 60 000
elements — the only difference being an unmodifiable wrapper the opcode refuses
to recognise as `RandomAccess`.

Two things this does **not** say. It is not a claim about `binarySearch`'s
asymptotics in isolation: the `ListIterator` fallback is `O(n)` per search
against `O(log n)`, and 60 000/log₂(60 000) ≈ 3 750, so a ~900x wall-clock ratio
is the algorithmic gap **damped** by per-element interpreter overhead that both
arms pay. And it is not a measurement of the other five `Collections` methods —
only `binarySearch` was run. `reverse` on these receivers throws
`UnsupportedOperationException` before the branch can matter, which is why
`binarySearch` is the honest one to price.

Incidental, and not this lane's to chase: **strict mode is 17x faster than
Compatible on the `plain` arm too** (2 ms vs 34 ms), on an operation with no
unmodifiable wrapper in it at all. That is a `--jdk-only`-is-better-than-
Compatible data point of the `G62-1` species, arrived at by accident.

## 3. The tier that did not get the fix — OPEN

`H18-1` lands the arm in `op_instanceof` and `op_checkcast`. **The JIT does not
share those.** Three sites carry the same disjunction and end it at
`synthetic_implements_public`, verified by grep at `9eef86699` rather than
inherited from `H15-2` §5.6:

```
vm/src/jit/helpers.rs:8502   (in `jit_typecheck_resolve`, site-recorded branch)
vm/src/jit/helpers.rs:8679   (in `jit_typecheck_resolve`, name-walk fallback)
vm/src/vm/vm_exec.rs:20309   (the reference-slot store predicate)
```

All three are outside this lane's ownership. **The consequence is not
cosmetic**, and it is the largest caveat on `H18-1`:

> `x instanceof RandomAccess` now answers **true** interpreted and **false**
> compiled. The branch that §2 prices lives **inside
> `java.util.Collections.binarySearch`** — a small, hot, obviously-compilable
> method. If it tiers up before the fix reaches the JIT, §2's 886x does not go
> away; it just becomes intermittent.

**An answer that flips when a method gets hot is worse than a consistently
wrong one.** `H15-2` §5.6 argues these sites are *already* less capable than the
interpreter (they lack the two `proxy_*` arms) so the asymmetry is pre-existing.
That is true and it is not a defence: pre-existing asymmetry in a predicate
nothing exercises is cheap; asymmetry in the predicate that selects
`binarySearch`'s algorithm is not.

### 3.1 The patch, written out — and the one thing that blocks a copy-paste

`jit_typecheck_resolve`'s signature is
`(vm: &SharedVm, obj_class_id: ClassId, obj_ref: &mut ObjectRef, class_name: &str, lenient: bool, …)`.
Two mismatches against `display_class_satisfies_target`:

1. **It works by NAME**, not by `ClassId`. Fine — `ClassManager` already has
   `is_subclass_of_by_name(child_id, target_name)`.
2. **It has no `&mut JvmThread`**, so it cannot pin the way the interpreter arm
   does. This is the real blocker and it must not be waved through. Note
   however that this function *already* survives a moving GC across a class
   load — its `obj_ref: &mut ObjectRef` carries the comment *"taken `&mut` so
   the slow path below can refresh the caller's copy after a GC-triggering
   class load"*. **Whoever ports the arm should reuse that existing refresh
   mechanism rather than importing `native_pin_roots`**, and should say in the
   commit which one they used and why.

A thread-free variant, to be added beside `display_class_satisfies_target` in
`vm/src/runtime/interpreter/typecheck.rs` (in this lane's ownership, but not
written, because a function with no caller is a fabrication of its own):

```rust
/// Name-keyed, load-free twin of [`display_class_satisfies_target`], for
/// callers that have no `JvmThread` to pin against (the JIT's
/// `jit_typecheck_resolve`).
///
/// DECLINES rather than loads when the display class is not resolved yet.
/// That makes it strictly weaker than the interpreter arm and the difference
/// is observable, so a caller adopting this owes the tier-differential
/// measurement `H7-1` N1 asks for and `RJitMapTierDiff` was built for.
pub fn display_class_satisfies_name_public(
    shared: &SharedVm,
    obj_ref: cratonvm_types::ObjectRef,
    obj_class_id: ClassId,
    target_class_name: &str,
) -> bool {
    let stamp_name = {
        let cm = shared.classes.class_manager.read();
        match cm.get_class(obj_class_id) {
            Some(c) if c.name.starts_with("cratonvm/internal/Unmodifiable") => c.name.to_string(),
            _ => return false,
        }
    };
    let Some(display_name) = unmod_stamp_display_name(shared, obj_ref, &stamp_name) else {
        return false;
    };
    if display_name == target_class_name {
        return true;
    }
    let cm = shared.classes.class_manager.read();
    match cm.get_loaded_class_id(display_name) {
        Some(cid) => cm.is_subclass_of_by_name(cid, target_class_name),
        None => false,
    }
}
```

and at each of the three sites, appended after the
`synthetic_implements_public` arm:

```rust
    if crate::runtime::interpreter::display_class_satisfies_name_public(
        vm, *obj_ref, obj_class_id, class_name,
    ) {
        return true;
    }
```

(`vm_exec.rs:20309` is a `||` chain returning `None` for "assignable", so there
the term joins that chain instead of being an early `return true`.)

**The `None => false` in the load-free variant is a deliberate, stated
weakness**, not an oversight. `H18-1` §5.1 rejects exactly this shape for the
interpreter because it makes the answer depend on whether anything called
`getClass()` first. Accepting it in the JIT trades one order-dependence for a
different one and is only defensible as an interim — which is why the doc
comment above says so out loud and why N1 below asks for the differential to be
measured rather than assumed away.

## 4. The verification agenda `H18-1` cannot run

This lane may not build. In order:

1. **`cargo check -p cratonvm-vm`** first. `H18-1` §8 lists the three likeliest
   compile failures. Nothing below is meaningful until this passes.
2. **The §1 probe, all three arms**, diffed on **stdout only** — `2>&1` puts the
   VM's `gc::guard` tracing into the diff and this probe provokes plenty of it.
   Expect: Compatible section A all `T/T`; section B's four casts `cast-ok`;
   both section-B negative controls unchanged; section D ratio near **1.0x**.
3. **`--jdk-only` 105/105.** Any movement in either direction falsifies
   `H18-1`'s claim that the arm is inert in strict mode.
4. **`SUITE=core`** — predicted **65/65** (`RImmutableFactoryTypes` green).
5. **`SUITE=all`** — predicted **101/105**, the other four standing failures
   unmoved.
6. **Then** re-run the §1 probe with the JIT warm (a driver loop that calls
   `Collections.binarySearch` enough times to compile it) and compare against
   the cold numbers. **This is the step that will show §3's tier split**, and
   nothing in the corpus asks for it today.

## 5. What I did NOT verify

* **Nothing in `H18-1`'s patch was compiled or run.** Every number on this page
  is from the **pristine** binary and describes the defect, not the fix.
* **The JIT twins were read, not run.** That a compiled `instanceof` answers
  `false` on these receivers is **ARGUED** from the three sites' source; no arm
  was run with the JIT warm to witness it. Step 6 above is that measurement and
  it does not exist yet.
* **Section D was run single-threaded on a shared host.** The *ratio* is robust;
  the absolute milliseconds are not, and should not be quoted.
* **`reverse`, `shuffle`, `fill`, `copy`, `swap`** were not priced. Only
  `binarySearch`.
* **The `List12`/`ListN` message residue** introduced by `H18-2` §1.1 is
  unmeasured — no run has yet produced a CCE naming a size-discriminated list
  or set form.

## NOMINATIONS

**N1 — port the arm to the three twins, and measure the differential while it is
still visible.** §3. `RJitMapTierDiff` (`regression-suite/src/`, landed
`e6adee5ee` for `H7-1` N1) is the instrument and this is the first defect that
gives it something unambiguous to see: run the §1 probe cold and JIT-warm on the
same binary and diff. **Do this before landing the port**, not after — once both
tiers agree, the differential is unmeasurable and the vector goes back to having
nothing to catch.

**N2 — a gate check for the `RandomAccess` contract, expressed as a ratio.**
§2's A/B needs no oracle column and no absolute threshold, which is what makes
it survivable on this host. Assert `binarySearch(unmodifiableList(x))` within,
say, 10x of `binarySearch(x)` in the same run. Today that assertion fails at
886x and would have failed silently for however long this has been true; a
value-only vector cannot express it. This is `H0-2` N2 with an instrument
attached.

**N3 — price the reachable `Collections` methods, and correct the two records
that list six.** `H0-2` and `H15-2` both name
`binarySearch`/`reverse`/`shuffle`/`fill`/`copy`/`swap` as if all six were
reachable on these receivers. **Four of them cannot be**: `reverse`, `shuffle`,
`fill` and `swap` mutate the list they are handed, so an unmodifiable receiver
raises `UnsupportedOperationException` before the `RandomAccess` branch can cost
anything.

The population that survives is **two**, and the second one is the interesting
one: `Collections.copy(dest, src)` mutates only `dest`, so `src` may legitimately
be a `List.of(...)`. Whether its fast-path branch actually reads *`src`*'s
`RandomAccess`-ness (rather than only `dest`'s) is **ARGUED from the API
contract here, not read out of the JDK source** — check it, then price it with
the same within-VM A/B as §2. If `copy` does read `src`, the blast radius is
`binarySearch` **and** every `copy` out of an immutable list, and the two
records should be corrected to name two methods rather than six. If it does not,
they should be corrected to name one.
