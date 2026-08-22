# H17-1 — the armed dial yields once per armed CLASS, not once per process; and two of the three witnesses used to measure it are now blind

> **CORRECTION 2026-08-22 — the counting claim in this record is
> withdrawn (`H17-2` N4).** The `table` array class is not a yield
> counter. It reports **one bit per map** — whether that map's FIRST
> insert ran bytecode — so it cannot distinguish "this method never
> yielded" from "this method yielded later, after the native had already
> allocated the table". Every observation here stands; every statement of
> the FORM "the dial yields once per X" does not.
>
> The mechanism is now settled and it was never a yield budget: the dial
> had one live call site of fourteen dispatch doors, so an armed class
> yielded only on the dispatches that reached step 1 cold. Fixed
> 2026-08-21 — all fourteen doors consult it, and an armed class now
> yields on every covered dispatch that has bytecode to yield to. See
> `WORKER-1-the-dial-now-reaches-every-door-20260821.md`.

**Status: MEASURED, and it corrects `H16-3` and `H0-8` on a point both stated as
settled.** Lane H17, 2026-08-21, on the prebuilt `C:/craton/cratonvm-r8.exe`
(2026-08-21 05:19, clean build at `025780ff7`). Oracle HotSpot 25.0.3+9.
**No source change was involved in any measurement here, and no suite was run.**

`H16-3` concluded the yield "happens exactly once, on the first `HashMap.put`
dispatch the process performs, and never again", and `H0-8` built on that as
"the single yield lands on whatever runs first". Both measured **one armed class
at a time**, which cannot distinguish *once per process* from *once per armed
class*. Arming two classes distinguishes them, and the answer is **per class**.

---

## 1. MEASURED — two armed classes get two yields

`YieldScope`: arm `java/util/HashMap` **and** `java/util/Hashtable` in one
process, populate one map of each with three puts, and read each backing
`table` reflectively (`--add-opens java.base/java.util=ALL-UNNAMED`). The
`table` ARRAY class is the witness — see §3 for why the head class is not.

Order is chosen by argv, so "which family yields" is separated from "which
family runs first", per `H0-8` §5.

```text
HOTSPOT           HashMap   cls=[Ljava.util.HashMap$Node;   len=16 real=3 fab=0
                  Hashtable cls=[Ljava.util.Hashtable$Entry; len=11 real=3 fab=0

UNARMED           HashMap   cls=[Ljava.lang.Object; len=16
                  Hashtable cls=[Ljava.lang.Object; len=16

ARMED both, HM first  [1st] HashMap   cls=[Ljava.util.HashMap$Node;   len=16
                      [2nd] Hashtable cls=[Ljava.util.Hashtable$Entry; len=11

ARMED both, HT first  [1st] Hashtable cls=[Ljava.util.Hashtable$Entry; len=11
                      [2nd] HashMap   cls=[Ljava.util.HashMap$Node;   len=16
```

**Both families get their real table, in both orders.** Under a once-per-process
yield the second family would have had to take the native's
`[Ljava.lang.Object;`. It does not. So the yield is **not** once per process.

## 2. MEASURED — but it is not once per triple either: it is once per CLASS

`TripleScope`, `java/util/HashMap` armed alone, four maps populated through
three DIFFERENT methods on the same class:

```text
[m1 put        ] table=[Ljava.util.HashMap$Node;
[m2 putIfAbsent] table=[Ljava.lang.Object;
[m3 merge      ] table=[Ljava.lang.Object;
[m4 put again  ] table=[Ljava.lang.Object;
```

`putIfAbsent` and `merge` are distinct triples and each is separately
registered, yet neither gets a yield of its own. So the scope is not the triple.

`ClassScope` is the order-swap control, and it is what makes this a result
rather than a coincidence of which method was written first:

```text
ARMED, put first          [1st] put         table=[Ljava.util.HashMap$Node;
                          [2nd] putIfAbsent table=[Ljava.lang.Object;

ARMED, putIfAbsent first  [1st] putIfAbsent table=[Ljava.util.HashMap$Node;
                          [2nd] put         table=[Ljava.lang.Object;
```

**The yield follows position within the armed class.** Whichever method touches
the class first gets it; every later method on that class, of any triple, does
not. Combined with §1: **one yield per armed class per process.**

## 3. MEASURED — `H16-3`'s head-class witness went blind, and `modCount` never was one

This matters more than the scope correction, because it decides what any
re-measurement can even see.

* **The head CLASS no longer discriminates.** `H16-3` classified buckets as
  "real `HashMap$Node`" vs "fabricated `AnonymousObject$4`". After `H16-2`
  landed, the **native mints a real `java.util.HashMap$Node` too** — the unarmed
  run above shows `real=3 fab=0` with `java.util.HashMap$Node` heads. A
  native-built table and a bytecode-built table are now **identical by head
  class**. Any re-run of `H16-3` §1's table on a current binary will read as
  "all real" and conclude the dial is fine.
* **`modCount` is not a witness.** It looked like the ideal per-dispatch probe —
  real `put` bytecode does `++modCount` on every structural insert. It is `4`
  after four puts **unarmed**, so the native maintains it as well:

  ```text
  HOTSPOT   [map1..3] modCount=4  sizeField=4  size()=4  table=[Ljava.util.HashMap$Node;
  UNARMED   [map1..3] modCount=4  sizeField=4  size()=4  table=[Ljava.lang.Object;
  ARMED     [map1]    modCount=4  sizeField=4  size()=4  table=[Ljava.util.HashMap$Node;
            [map2]    modCount=4  sizeField=4  size()=4  table=[Ljava.lang.Object;
            [map3]    modCount=4  sizeField=4  size()=4  table=[Ljava.lang.Object;
  ```

* **What survives is the `table` ARRAY class**, `[Ljava.util.HashMap$Node;`
  versus `[Ljava.lang.Object;`. It is a real witness but a **coarse** one: the
  array is allocated once per map, so it reports only whether that map's FIRST
  insert ran bytecode. It cannot count yields within a map.

**Consequence, ARGUED:** the standing note *verify what the instrument measures
before believing it* applies to this directory's own witness. `H16-3` §1's
`n`-row table is not reproducible as written on a current binary, and a reader
who re-runs it will get a false green.

## 4. MEASURED — the `--jdk-only-report` census cannot count yields, and it under-reports the native-won half

Run armed for `java/util/HashMap`, three maps, four puts each — twelve puts, of
which §2 says one ran bytecode and eleven ran the native.

The report carries **exactly one row** for the triple, with no count field:

```json
{"kind":"native-shadows-bytecode","class":"java/util/HashMap","method":"put",
 "descriptor":"(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
 "native_kind":"bridge","outcome":"bytecode-won"}
```

and **no `bridge-ran-over-bytecode` row for `HashMap.put` at all**, though
eleven of the twelve puts took the native. `grep -c` for the triple returns `1`.

Two separate facts here:

1. **The sink is a deduplicated presence filter, not a counter.** Every row is
   one distinct `(class, method, descriptor, tag)` digest. "Appears in the
   census" means *at least once*, never *how often*.
2. **Under `enforce`, the native-won outcome is never recorded.** In
   `resolve_step1_native` the recorder is guarded
   `if shadows_bytecode && !enforce`, and the yield path records via
   `resolve_native_dispatch_wave1`. So when the dial is ARMED and the native
   wins anyway, **nothing records it** — which is exactly the eleven puts above.

## 5. MEASURED-by-elimination — the false input is `step1_dispatch_has_code`, not the observation sink and not the dedup

`resolve_step1_native` computes

```rust
let ask = strict_bridge && (enforce || !jdk_only_shadow_already_observed(..));
let shadows_bytecode = ask && step1_dispatch_has_code(..);
```

`enforce ||` short-circuits, so **when the dial is armed `ask` is
unconditionally `true`** and the observation dedup cannot gate it. The doc
comment's stated intent — *"when the shadow is being ENFORCED the answer is a
dispatch input and must be current, so the walk runs every time"* — **holds as
written.** The brief's hypothesis 1 is DISPROVED, and so is the suggestion that
the sink's saturation is involved: `jdk_only_shadow_already_observed` is not
consulted at all on the armed path.

Therefore, for the eleven puts that took the native with `enforce == true`,
`shadows_bytecode` must have been `false`, and the only remaining factor is
**`step1_dispatch_has_code` returning `false` on the second and later
dispatches.** It holds no memo of its own, so the state it depends on is one of
its inputs:

* `dispatch_class_override` — supplied by the call site, `None` on a cold site
  and `Some(id)` once the site has resolved one. The two are not obliged to
  name the same class, and the function starts its walk at whichever it is
  given.
* the `MemberResolver` / per-VM `LinkResolver` that `declared_method` both
  consults and **populates** — its own doc comment says it "populates the per-VM
  `LinkResolver` with the resolution the invoke about to happen will ask for".

**This is a deduction from the census plus §2, not an instrumented observation.
I did not build, so I did not confirm which of the two inputs flips.** It is
stated as the narrowed suspect, not as the cause.

## 6. Incidental, MEASURED — `Hashtable` unarmed is built by `HashMap`'s minter

Visible in §1's unarmed row and not otherwise filed: unarmed `Hashtable` gets a
**length-16** table whose heads are **`java.util.HashMap$Node`**. HotSpot gives
length **11** and `java.util.Hashtable$Entry`. Armed, it gets the real
`[Ljava.util.Hashtable$Entry;` len 11 — and a MIX, one `HashMap$Node` head
alongside one `Hashtable$Entry` head, which is `H16-3`'s hybrid seen with the
one witness that still works.

This is `native-collections/src/lib.rs`, which lane **H23** owns. Not touched
here. It compounds `H16-3` N3 (`Hashtable` also uses `HashMap`'s bucket index).

## 7. What I did NOT verify

* **I did not build**, so nothing here tests a fix, and §5's mechanism is
  narrowed by elimination rather than instrumented.
* **I did not re-run any suite**, so no acceptance number in this directory is
  re-measured or challenged by this record.
* **I did not test a third armed class**, so "one per class" is measured on two
  and is a generalisation from two points plus the within-class control in §2.
* **I did not test `all`**, a non-`java/util` prefix, or any of `H14-3`'s
  thirteen registrars.
* **I did not establish the yield's scope for reads** as opposed to inserts, so
  `H0-8` §3's polarity asymmetry is NOT explained here — see `H17-2`.

## NOMINATIONS

* **N1 — `H16-3` §1 and §3's tables should be marked non-reproducible.** Their
  witness is the head class, and §3 above shows it stopped discriminating when
  `H16-2` landed. The correction is one line in each, not a re-measurement.
* **N2 — the per-class scope changes the price, and in the expensive
  direction.** Every armed cell in `H0-4`, `H0-3` and `H14-3` was read as "one
  contaminated operation in the process". It is one contaminated operation **per
  armed class**, and `H14-3`'s registrar arms arm many classes at once. The
  hybrid surface is wider than `H16-3` said, not narrower.
* **N3 — the census needs a native-won row under `enforce`.** §4.2: the one
  configuration whose whole purpose is to measure shadowing is the one that
  records only the half that yielded. This is in `resolve_step1_native`, which
  H17 owns, and is the smallest useful change in the file.
* **N4 — give the sink counts, or stop reading it as a quantity.** §4.1. The
  rows are a set, and at least one page in this directory quotes census row
  counts as if they were dispatch counts.
