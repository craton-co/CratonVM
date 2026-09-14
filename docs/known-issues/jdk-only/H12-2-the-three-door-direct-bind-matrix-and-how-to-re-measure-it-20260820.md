# H12-2 — the three-door direct-bind matrix: every thin native bind, its registry kind, and which door asks a policy question

**Status: REFERENCE.** A table and a reproduction recipe, extracted so the next
lane does not re-derive them. The finding they support is `H12-1`; this record
is the working-out.

**Provenance.** Door/ladder rows are ARGUED from source, each with a file and
line, at branch `claude/jdk-only-mode-handoff-09b48c` `fe59bf9d9`. Registry
kinds are MEASURED from `scripts/baselines/jdk-only-kind-map-25-linux.tsv`, a
checked-in census, **not** a live registry dump on this host. §3's numbers are
MEASURED on `C:/craton/target-jdkonly-h2/release/cratonvm.exe` (built at
`fe59bf9d9`, i.e. *without* H12-1's fix). Lane H12, 2026-08-20.

---

## 1. The matrix

`jit/src/compile_gate.rs`'s module doc names three doors. A fourth column would
be needed the day someone adds one, which is `H12-1` N1's point.

"Guarded" means the bind is decided by `jit::direct_native_helper` (or
`direct_native_helper_for_impl`), which under `jdk_only` admits only
`NativeKind::Intrinsic` and records a counted refusal otherwise.

| triple | kind | MethodEntry, single-pass | MethodEntry, IR | **OSR** | **EagerFirstCall** |
|---|---|---|---|---|---|
| `java/lang/StringLatin1.toLowerCase(String,[B,Locale)` | `intrinsic` | guarded `lib.rs:19461` | — | — | — |
| `java/lang/Thread.currentThread()` | **`bridge`** | guarded `19500` | guarded `17757` | **UNGUARDED `jit_bridge.rs:959`** | — |
| `jdk/internal/util/Preconditions.checkIndex(II,BiFunction)` | **`bridge`** | guarded `19549` | guarded `17795` | **UNGUARDED `1006`** | — |
| `java/lang/ref/Reference.reachabilityFence(Object)` | **`bridge`** | guarded `19582` | guarded `17817` | **UNGUARDED `1028`** | — |
| `java/lang/Integer.valueOf(I)` | `intrinsic` | guarded `19616` | — | unguarded `1061` | unguarded `interpreter.rs:2611` |
| `java/lang/Integer.intValue()` | `intrinsic` | guarded `19944` | — | unguarded `1136` | unguarded `2635` |
| `java/util/concurrent/ConcurrentMap.get` → impl `ConcurrentHashMap.get` | **`bridge`** / **`bridge`** | guarded-for-impl `19979` | — | not bound | — |
| `java/util/HashMap.put(Object,Object)` | **`bridge`** | guarded-for-impl `20054` | — | **UNGUARDED `1173`** | — |
| `java/util/HashMap.get(Object)` (and `java/util/Map.get` → impl `HashMap`) | **`bridge`** | guarded-for-impl `20076` | — | **UNGUARDED `1173`** | — |
| `java/lang/Math.sqrt(D)D` | n/a — inline `MATH_SQRT_INTRINSIC`, no registered-native shadow | — | — | `912` | `2581` |

Reading the table:

* **Five bold cells.** Five `bridge` rows bound at the OSR door with no policy
  question — the live defect in `H12-1` §3.
* **The two `intrinsic` rows bound at OSR and Eager are correct today and
  unguarded.** They are inside §1.4's reviewed exception, so nothing is wrong;
  but nothing *asks*, so the correctness is a property of the current tagging
  rather than of the code. That is the same shape `H7-1` §4a found one layer up,
  and it is why `H12-1` N1 asks for a type-level obligation rather than a third
  hand-copy of the question.
* **The `ConcurrentMap` row is bound at no direct door**, so `H7-1` §4a's fix is
  a backstop rather than a live change — which is exactly what `H7-1` says.
* **`Math.sqrt` is not in this population at all**: it lowers to `SQRTSD`, it
  does not shadow a registered native, and no policy question is owed. Listed so
  a future reader does not "fix" it.

### 1a. Why a call-site grep could not have produced this table

Three separate reasons, all of which cost time this round:

1. `direct_native_helper` refuses at **bind** time, one crate away from the
   helper — `H7-1`'s finding. A grep at the helper sees nothing.
2. The two direct doors bind by **raw function address**
   (`crate::jit::helpers::NAME as *const () as usize`), not through the
   `*_DIRECT_FN` cell, so a grep for the cell name misses them entirely. That is
   how the OSR HashMap binds stayed out of `H4-1` §1c's six-row census and out
   of `H7-1`'s follow-up.
3. The doors are three files in two crates (`jit/src/lib.rs`,
   `vm/src/runtime/interpreter/jit_bridge.rs`,
   `vm/src/runtime/interpreter.rs`). Only `compile_gate.rs`'s doc table names all
   three in one place, and it is not where anyone looks for native policy.

The greps that *do* work, for whoever re-takes this:

```bash
# every direct bind, all doors: the two shapes side by side
grep -n 'direct_native_helper\|_DIRECT_FN,' jit/src/lib.rs
grep -n 'helpers::jit_[a-z_]*_direct as \*const ()' \
     vm/src/runtime/interpreter/jit_bridge.rs vm/src/runtime/interpreter.rs
# then the kind of each triple
grep -P '^java/util/HashMap\t(get|put)\t' scripts/baselines/jdk-only-kind-map-25-linux.tsv
```

---

## 2. The probes

Three, in increasing specificity. All are `javac`-only; none needs a Rust build.
Resolve the JDK rather than copying a path from a record —
`JDK="$(dirname "$(dirname "$(command -v javap)")")"` gave
`/c/Program Files/Microsoft/jdk-25.0.3.9-hotspot` (25.0.3+9) on this host.

**The one property every one of them must have**: the hot loop lives in a method
invoked **exactly once**, so its body can only be compiled by the OSR door. A
loop in a method called many times goes to the MethodEntry door, where the guard
already refuses, and the probe then measures the guard working.

* **`H12Door.java`** — the door discriminator. A single-invocation loop
  containing `invokestatic Thread.currentThread()` (the only OSR bind in the
  matrix that is *counted*) plus exact-`HashMap` `get`/`put`. This is the probe
  that produced `H12-1` §3b.
* **`H12Osr.java`** — three key shapes (`Integer`, `String`, a custom
  `hashCode()`), 1024 keys, plus a full cold re-read after the loop.
* **`H12Split.java`** — one map object, two receiver declarations:
  `HashMap`-declared for `put`/`get` (bound at the OSR door) and `Map`-declared
  for a second `get` (not recognised at that door, so it takes the
  policy-checked dispatcher). The in-process route split.

The last two are the *negative* results in `H12-1` §4 and are worth keeping
precisely because they are negative: they establish that a store-then-read
vector cannot see this defect, which is the trap `RJitMapTierDiff` has to avoid.

Sources are in this record's sibling directory only as text below, because
`regression-suite/` belongs to lane H10 and this lane was not to write there.
`H12Door.java`, the load-bearing one:

```java
import java.util.HashMap;

public class H12Door {
    static long hot(int n) {
        HashMap<Integer, Integer> mi = new HashMap<>();
        long acc = 0;
        for (int i = 0; i < n; i++) {
            int k = i & 255;
            mi.put(k, i);
            Integer a = mi.get(k);
            acc += (a == null ? -1 : a);
            if (Thread.currentThread() == null) { acc++; }   // door witness
        }
        System.out.println("size=" + mi.size());
        return acc;
    }
    public static void main(String[] args) {
        System.out.println("acc=" + hot(Integer.parseInt(args[0])));
    }
}
```

Confirm the call sites survived `javac` before trusting a run — the OSR ladder
matches `invoke_kind == 0 && target_class == "java/util/HashMap"`, so a
`Map`-declared receiver silently tests nothing:

```bash
javap -c -p H12Door | grep 'invokevirtual.*java/util/HashMap'
```

---

## 3. The recipe, and the three numbers that matter

```bash
CRATONVM_INTRINSIC_STATS=1 cratonvm.exe --jdk-only        -cp . H12Door 300000
CRATONVM_INTRINSIC_STATS=1 cratonvm.exe --real-jdk        -cp . H12Door 300000
CRATONVM_INTRINSIC_STATS=1 cratonvm.exe --jdk-only --nojit -cp . H12Door 300000
```

The line to read is emitted by `vm-cli/src/main.rs:5091`:

```
[cratonvm] compiled Thread.currentThread direct calls: <calls>
  (sites bound per compile door: single-pass <sp>/<seen>, IR <ir>/<seen>, OSR <osr>; …)
```

MEASURED at `fe59bf9d9`, i.e. **before** `H12-1`'s fix:

| arm | calls | single-pass | IR | OSR |
|---|---|---|---|---|
| `--jdk-only` | **298 000** | 0/7 | 0/0 | **1** |
| `--real-jdk` | **298 000** | 0/0 | 0/0 | **1** |
| `--jdk-only --nojit` | 0 | 0/0 | 0/0 | 0 |

What each column is for:

* **`single-pass 0/7`** is the positive control. The MethodEntry door examined
  seven sites and bound none, so the guard exists and fired — the check
  `verify-the-jit-ban-exists-before-ab-testing-it` demands, done before drawing
  any conclusion from the OSR column.
* **`OSR 1` with 298 000 calls** is the defect.
* **`--real-jdk` identical to `--jdk-only`** is the sharpest form of it: the door
  produces the same bind profile in both modes, i.e. it never asked.
* **`--nojit` all zeros** is the negative control.

After the fix, the expected shape — **PREDICTED, not measured, since no binary
carrying `9b7ad0f07` exists**:

| arm | calls | OSR sites | `jit_direct_helper_jdk_only_refusals()` |
|---|---|---|---|
| `--jdk-only` | **0** | still **1** | **> 0**, ≈ 298 000 |
| `--real-jdk` | 298 000 | 1 | 0 |

`OSR` staying at **1** is deliberate and is the falsifier to watch: `H12-1`'s fix
is callee-side, so the door still *binds* and the helper declines at the call.
An OSR count that dropped to 0 would mean something else changed. Once O1 lands
(the bind-time refusal), `OSR` goes to 0 and the refusal counter goes to 0 with
it — and at that point a run cannot tell "the bind-time gate worked" from
"nothing reached the door" unless N2's per-door counter exists. That ordering is
the reason N2 is not optional.

---

## 4. What this record does NOT establish

* That any of it compiles. No build was run, by instruction.
* That the OSR door's HashMap binds ever fire in any *suite* vector. I measured
  a purpose-built probe. `H7-1` §6b's survey of the corpus was about the
  MethodEntry ladder and does not answer this for the OSR door, and no counter
  exists that could (N2 in `H12-1`).
* Any value divergence. See `H12-1` §4: none witnessed, and the reason —
  `native_hashmap_get_exact` walks the real `table` field
  (`native-collections/src/lib.rs:9222`) — is why a store-then-read probe never
  will.
* Whether the overlay (`try_hm_int_fast_*`) was reached by any run here. It was
  not instrumented and I make no claim either way.

---

## 5. NOMINATIONS

**N1 — this table should be generated, not written.** Every cell is derivable:
the bind sites are three greps (§1a), the kinds are one join against the
checked-in TSV. A table maintained by hand is a table that is wrong within a
week — `a-triage-page-is-stale-the-day-after-it-is-written`. A ~40-line script
under `scripts/` emitting exactly §1, run in the same place the kind-map ratchet
runs, would make "a new direct bind appeared and nobody asked its kind" a diff
rather than an audit. Deliberately not written here: `scripts/` is out of this
lane's ownership.

**N2 — `static_sites_seen()`'s denominators disagree between modes and nobody
knows why.** §3's `single-pass 0/7` under `--jdk-only` versus `0/0` under
`--real-jdk` means the single-pass ladder examined seven `invokestatic` sites in
one mode and none in the other, on the same program. The numerator is 0 in both,
so `H12-1` does not depend on it — but a denominator that moves with the
execution policy is either a real difference in what gets single-pass compiled,
or a counter that is not measuring what its label says. One `CRATONVM_DBG` run
would settle it, and until it is settled, that denominator should not be quoted
as "sites this ladder examined" in any argument.
