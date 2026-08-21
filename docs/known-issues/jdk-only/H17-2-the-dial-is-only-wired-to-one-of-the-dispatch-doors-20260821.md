# H17-2 — the dial is wired to ONE dispatch door, so "arming a class" only ever affects the calls that reach step 1 cold; and I am retracting H17-1 §2's mechanism

**Status: OPEN — MEASURED, with the mechanism ARGUED and explicitly not
settled.** Lane H17, 2026-08-21, prebuilt `C:/craton/cratonvm-r8.exe` (clean
build at `025780ff7`). Oracle HotSpot 25.0.3+9. **No source change, no suite
run, no build.**

This record does three things: it **disproves three hypotheses** (one of them my
own, from `H17-1`), it reports the one experiment that discriminates sharply,
and it states what is still unknown rather than closing the question.

---

## 1. RETRACTION — `H17-1` §2's "one yield per armed CLASS" is not supported

`H17-1` concluded, from the `table` array class, that the yield is once per
armed class. **The census contradicts it in the same run**, and I did not check
the census against the witness before publishing.

Armed for `java/util/HashMap`, `ClassScope`, put-first. The witness says
`putIfAbsent`'s map got a native-shaped table:

```text
[1st] put         size=4 table=[Ljava.util.HashMap$Node;
[2nd] putIfAbsent size=4 table=[Ljava.lang.Object;
```

The census for **the same process** says `putIfAbsent` won bytecode anyway:

```text
"class":"java/util/HashMap","method":"<init>",     "outcome":"bytecode-won"
"class":"java/util/HashMap","method":"put",        "outcome":"bytecode-won"
"class":"java/util/HashMap","method":"putIfAbsent","outcome":"bytecode-won"
"class":"java/util/HashMap","method":"size",       "outcome":"bytecode-won"
```

**Four distinct triples on one class each won bytecode at least once.** A
once-per-class yield cannot produce four. So:

* `H17-1` §2's **observable stands** — one real `Node[]` table per armed class,
  order-swap controlled. What it means does not.
* `H17-1` §2's **mechanism claim is withdrawn.** "One yield per armed class" is
  false.
* The `table` witness reports on **each map's first insert only**, so it can
  never distinguish "this method never yielded" from "this method yielded on a
  later call, after the native had already allocated the table". I read the
  first as if it were the only reading.

`H16-3`'s "once per process" and `H0-8`'s "the single yield lands on whatever
runs first" inherit the same defect: all three of us read a first-insert bit as
a yield count.

## 2. MEASURED — a third witness is also blind, so the effect is close to unmeasurable from inside the JVM

Adding to `H17-1` §3's two (head class, `modCount`), I tried per-key
`hashCode()` counting. Real `HashMap.put` bytecode calls `hash(key)` exactly
once per put, so the count should separate bytecode puts from native puts.

```text
HOTSPOT   [map1..3] puts=4 hashCode=4 equals=0  table=[Ljava.util.HashMap$Node;
UNARMED   [map1..3] puts=4 hashCode=4 equals=0  table=[Ljava.lang.Object;
ARMED     [map1]    puts=4 hashCode=4 equals=0  table=[Ljava.util.HashMap$Node;
          [map2..3] puts=4 hashCode=4 equals=0  table=[Ljava.lang.Object;
```

**`hashCode` is 4 in every configuration.** The native calls the key's
`hashCode` exactly once per put, like the bytecode.

Running total of witnesses tried, **all MEASURED**:

| witness | discriminates native vs bytecode put? |
|---|---|
| bucket head class (`H16-3`'s) | **no** — native mints a real `HashMap$Node` since `H16-2` |
| `modCount` field | **no** — native maintains it |
| key `hashCode()` call count | **no** — native calls it once per put |
| `equals()` call count | **no** — zero in all three |
| `table` ARRAY class | **yes, but one bit per MAP**, not per dispatch |
| `--jdk-only-report` census | **presence only**, never a count (`H17-1` §4) |

**ARGUED:** this is not bad luck. The native is a faithful imitation on every
observable a Java program can reach, which is exactly why the unarmed VM passes
suites. The one thing it does not imitate is the *type of the array it
allocates*, and that is the only reason any of this was visible at all.

## 3. MEASURED — `anewarray` is NOT the latch (a hypothesis worth killing)

Before blaming dispatch, I checked whether the "first one is right, the rest are
`Object[]`" shape belongs to reference-array allocation itself, which would have
made the whole dial story a misattribution of `H0-6` §10 / `H16-3` N4. No dial,
no `HashMap`, no collections native:

```text
HOTSPOT   [str 1..3] [Ljava.lang.String;   [node 1..3] [LAnewarrayLatch$Node;   [intg 1..3] [Ljava.lang.Integer;
CRATONVM  [str 1..3] [Ljava.lang.String;   [node 1..3] [LAnewarrayLatch$Node;   [intg 1..3] [Ljava.lang.Integer;
```

**Identical to HotSpot on every repeat.** `anewarray` resolves its component
type correctly and does not degrade. So when real `HashMap` bytecode runs, it
really does build a correctly-typed table, and `[Ljava.lang.Object;` really is
the native's signature. The witness in §1 is sound as far as it goes.

## 4. MEASURED — the sharpest result: a door that does not honour the dial still uses up whatever the dial had to give

`ReflectOrder`, armed for `java/util/HashMap`. One map filled by four direct
`m.put(...)` calls, one by four `Method.invoke` calls. Order chosen by argv, so
door is separated from position per `H0-8` §5.

```text
ARMED, direct first    [1st] direct  table=[Ljava.util.HashMap$Node;
                       [2nd] reflect table=[Ljava.lang.Object;

ARMED, reflect first   [1st] reflect table=[Ljava.lang.Object;
                       [2nd] direct  table=[Ljava.lang.Object;     <-- consumed
```

Read the second block. The reflective calls **did not take bytecode** — and the
direct call site that follows, which takes bytecode when it goes first,
**also does not**. The reflective door neither honours the dial nor leaves it
for the door that would.

This is the cleanest discriminator I found, and it is the one that names a
mechanism.

## 5. ARGUED — the model that fits, and the two it replaces

`force_native_over_real_jdk_bytecode_memoized`'s own doc comment says reflective
`Method.invoke()` dispatch "has no bytecode PC to key an invoke-cache entry on",
and lists it alongside megamorphic sites, `invokespecial` and interface-default
dispatch as paths that reach
`should_force_registered_native_over_bytecode` **instead of** step 1.

The dial is read in exactly one place: `resolve_step1_native`, via
`jdk_only_enforce_shadow_for(class_name)`. **No other door consults it.**

So the model is: **arming a class does not arm the class. It arms the subset of
that class's dispatches that reach step 1**, and every dispatch served by a warm
invoke-cache entry, by the force-native interceptor, or by reflective invoke
runs the native regardless of the dial. §4 is that model's prediction, tested
and confirmed in both orders.

This **replaces** two earlier explanations:

* **`H16-3` §2a's inline-cache memo** — same family of cause, but it predicts
  once-per-*resolved-method*, and §1 shows four triples on one class each
  winning bytecode, while §4 shows a door that consumes without honouring.
  Closer than it was given credit for; not exactly right.
* **`H17-1` §5's `step1_dispatch_has_code`** — I deduced it must be returning
  `false` on later dispatches. That deduction assumed the later dispatches
  *reached* step 1. §4 says they need not. **The function is no longer the prime
  suspect; the routing to it is.**

**This is ARGUED. I did not build and did not instrument any door.** What §4
establishes is a behavioural fact about the reflective door; the extension to
warm invoke-cache entries is inference from the tree's own prose plus §1.

## 6. Why I did not ship a fix

The brief asks for the dial to yield on every armed dispatch. I am not making
that edit, and the reason is the point of this record.

1. **The brief's hypothesis 1 is disproved.** `ask` is
   `strict_bridge && (enforce || !already_observed)`; `enforce ||`
   short-circuits, so the dedup provably cannot gate the enforcement path
   (`H17-1` §5). The comment's stated intent holds as written. There is no
   one-line bug there to fix.
2. **The brief's hypothesis 2 is disproved for `env_cache.rs`.** Its `OnceLock`s
   latch the *scope string*, which is static configuration and correct to latch.
   `jdk_only_enforce_shadow_for` returns the same answer on every call.
3. **The remaining change is not small.** Making every door honour the dial
   means teaching the force-native interceptor, the cached-dispatch path and
   the reflective path to consult it — hot paths, several of which carry
   explicit warnings about per-call-site behaviour drift (the `java/lang/String`
   arm deleted on 2026-08-04 "because a method's behaviour started depending on
   how many times its call site had run").
4. **I cannot verify.** Acceptance is three suite runs; I may not build. An
   unverified edit to dispatch routing, landed blind, is the failure mode this
   directory already has a note for.

**A precise diagnosis that survives is worth more than a blind edit that does
not.** The diagnosis is §4 and §5.

## 7. What this does to the project's numbers — and the direction, with a falsifier

The brief asked for a prediction. **ARGUED:**

* Every armed measurement in this directory (`H0-4`'s six families, `H0-3`'s
  eleven, `H14-3`'s thirteen arms) priced **a fraction of the class's
  dispatches** — the cold step-1 ones — not the class.
* Therefore the armed cells are **optimistic**: a real retirement removes the
  registration, so *all* doors miss and *every* dispatch runs bytecode. Arming
  today leaves the warm, reflective and force-native doors on the native.
* **Prediction: once the dial reaches every door, armed pass counts go DOWN**
  (more vectors fail), and the "five free registrars" of `H14-3` stop being
  free.
* **Falsifier:** if a fixed dial leaves `HashMap` at or above 81/104, then the
  cold step-1 dispatches were already the overwhelming majority, this whole
  correction is a rounding error, and `H0-4`'s table can be quoted as-is.
* **`H16-3`'s hybrid argument survives and gets worse.** A fully-armed class
  still mixes bytecode-built and native-built state whenever any door is missed
  mid-structure, so the hybrid caveat is not repealed by fixing the dial —
  only by retiring the registration.

## 8. What I did NOT verify

* **I did not build, instrument, or run any suite.** No acceptance number is
  measured or challenged here; `105/105`, `103/105`, `65/65` are untouched and
  unconfirmed by me.
* **I did not read the invoke cache's or the force-native interceptor's code
  paths end to end**, so §5's extension beyond the reflective door is inference.
* **I did not test the warm-invoke-cache door in isolation** — that needs either
  a build or a reliable way to force a call site megamorphic, and I did not
  find one that the coarse witness could read.
* **I did not test `all`, a non-`java/util` prefix, or any `H14-3` registrar.**
* **`H0-8` §3's polarity asymmetry is still unexplained.** §1 removes the
  "exactly one differs" framing that made it puzzling — if yields are per-door
  and not per-position, "puts fail first, reads succeed first" may not be one
  phenomenon at all. I did not measure the read side.
* **I did not confirm that `putVal` / `resize` are unregistered**, which one
  candidate sub-model in §1 turns on.

## NOMINATIONS

* **N1 — instrument the doors before changing any of them.** One counter per
  door (step 1, force-native interceptor, cached dispatch, reflective) recording
  armed `Bridge` dispatches and whether the dial was consulted. That single
  build answers §5 outright and turns every ARGUED claim here into a number. It
  is the next action for this lane and it needs nothing but a build.
* **N2 — `H17-1` §2 is retracted by §1 of this record.** Anyone quoting "one
  yield per armed class" should quote §1 instead. The observable is intact; the
  mechanism is not.
* **N3 — the witness table in §2 belongs in `scripts/jdk-only-blast-radius.sh`.**
  Four of six witnesses this directory has reached for do not discriminate on a
  current binary. A reader who picks one at random gets a false green.
* **N4 — `H16-3` §1 and §3 and `H0-8` §3 all read a first-insert bit as a yield
  count.** Same correction as §1, applied to their tables.
* **N5 — decide the dial's contract and write it down** (`H16-3` N2, unresolved
  and now sharper). "Yield once per cold step-1 dispatch" is what it does;
  "simulate a retirement" is what four records read it as. Those differ by every
  warm dispatch in the process.
