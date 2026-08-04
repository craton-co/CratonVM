# Fabricated object layouts leak into native code — index-based field access breaks silently when the class becomes real

**Status:** OPEN — JDK-only wave-2 work item, filed 2026-07-31, re-verified
against the re-landed tree the same day. **DANGEROUS: every instance is a
silent wrong-field read or write, never an exception.**

> **Evidence provenance.** The `drop_real_layout_synthetic` doc and all ten
> `JDK-ONLY-LAYOUT` markers are pre-existing or re-landed code and were read
> from `C:\craton\wt-jdk-only` (branch `feat/jdk-only-mode`) on 2026-07-31.
> Two corrections to the original filing are folded in below: the drop list
> names **six** drift families, not five, and the marker table's per-file
> verdict split was slightly off.

## What changed on 2026-08-04 — step 2 of four

*What specifically must change* lists four steps. Step 2 — **adjudicate the two
`unknown` verdicts in `vm/src/vm/vm_object.rs`** — is now partly answered, with
evidence rather than an argument.

Both are **overlays**, not mis-numbered slots: a VM-internal `Int` deliberately
written on top of `java.lang.Class`'s instance field 0, which JDK 25 declares as
`Constructor<T> cachedConstructor` — a *reference* slot. The question was never
"is this the right slot" but "does writing an `Int` where the image declares a
reference corrupt anything". The marker listed three checks. Two are run,
against a real JDK 21 image, and both are clean:

1. Three rounds of `getDeclaredConstructor()` on a nested class, interleaved
   with `String.class.getConstructor(String.class)` — so the second and third
   take the real bytecode's `cachedConstructor != null` fast path — all returned
   the right `Constructor`. Nothing raised, and no `expected object reference,
   got int(N)`.
2. The same run under `CRATONVM_DBG=overlay`, whose hunter
   (`overlay_write_is_destructive`) exists precisely to report a primitive
   written to a reference slot, reported nothing.

So the verdict stays `unknown` but **drops from ranked-HIGH**: the two checks
that would have shown live harm did not.

The third check — does anything still *depend* on the overlay — is the one whose
answer removes code rather than reassuring about it, and nothing in the tree
could answer it. `mirror_class_id` (`native-builtins/src/lang_class.rs`) is the
overlay's only reader outside the VM, a fallback behind the reverse map, and it
now reports its first hit under the same flag. **One broad real-JDK run makes
the verdict decidable.** A ten-class probe does not: silence over a small
workload is not silence over Spring Boot, and the marker says so rather than
inviting a deletion on thin evidence.

The primitive-mirror sibling (`Int(-1)` over the same slot) rides on that
finding: it is the easier of the two to retire if check 3 comes back zero,
because a primitive mirror has no legitimate `cachedConstructor` reader at all.

## What is still open — steps 1, 3 and 4, which are the bulk

* **Step 1, the sweep — it has a measured work list now (2026-08-04).** The
  sweep was scoped as "read four crates for index-based field access". It does
  not need reading first: **the runtime detector for exactly this defect already
  exists** and had never been run broadly. `CRATONVM_DBG_OVERLAY=1` reports a
  native writing a primitive to a reference slot *or* a reference to a primitive
  slot on a class loaded from real JDK bytes. Add `CRATONVM_DBG_OVERLAY_ALL=1`
  or the `java.util.Map` suppression hides the dominant family.

  Three probes, both modes, JDK 25. **The results are identical under
  `--real-jdk` and `--jdk-only`**, so this is a `Compatible`-mode defect too.
  Distinct `(class, slot, value kind, real descriptor)` sites:

  | class | slot | writes | real desc | n |
  |---|---:|---|---|---:|
  | `java/util/HashMap` | 1 | `Int` | `L` | 4,395 |
  | `java/util/HashMap$Node` | 2 | `Int` | `L` | 2,108 |
  | `java/lang/invoke/VarHandle` | 1 | `Object` | `Z` | 52 |
  | `java/lang/invoke/VarHandle` | 0 | `Int` | `L` | 52 |
  | `java/util/HashMap` | 2 | `Int` | `[` | 32 |
  | `java/lang/invoke/MemberName` | 4 | `Int` | `L` | 14 |
  | `java/util/Properties` | 7 | `Float` | `L` | 6 |
  | `java/util/Properties` | 6, 5 | `Int` | `L` | 6 each |
  | `java/util/Properties` | 2 | `Object` | `I` | 6 |
  | `ClassLoaders$PlatformClassLoader` | 0, 3, 4, 6 | `Int` | `L` | 4 each |
  | `ClassLoaders$AppClassLoader` | 0, 3, 4, 6 | `Int` | `L` | 4 each |
  | `java/util/Scanner` | 3, 4 | `Int` | `L` | 2 each |
  | `java/net/URI` | 5 | `Object` | `I` | 2 |
  | `java/net/URI` | 2 | `Int` | `L` | 2 |
  | `java/net/Proxy` | 0 | `Int` | `L` | 2 |

  **13 classes, 24 distinct slots**, from three small probes. Reading it:

  * The **`HashMap` family is the known-benign case** the hunter suppresses by
    default — coercion-to-null lands the real bytecode in the null-initialised
    state it expects. It dominates by volume and says nothing.
  * **`VarHandle` is the one to look at first.** It mismatches in *both*
    directions on adjacent slots (`Int` over a reference at 0, an `Object` over
    a `boolean` at 1), it is not a `Map`, and the benign argument says nothing
    about it.
  * **`Properties` writing a `Float` over a reference slot** is in the group
    item 1 calls the highest-risk in `native-collections`, and it is on the
    bootstrap path.
  * **Both built-in class loaders take `Int` writes over four reference slots
    each.** Whatever those slots hold on the real classes, they are not integers.
  * `URI` and `Properties` each mismatch in both directions, which rules out a
    single off-by-one against one layout.

  Two limits, so nobody reads this as complete. The detector covers
  `NativeContextImpl::set_field` only: **reads are uninstrumented, and a
  same-kind wrong-slot write is invisible** — an `Int` into the wrong `Int` slot
  passes silently, and that is half the defect this record describes. And three
  probes is not Spring Boot. Treat the table as a floor and re-run under H2 or
  Spring Boot before calling the sweep done.

  Adjudicating and fixing the 24 sites is untouched.
* **Step 3**, replacing the two `breaks-under-strict` sites in `vm_util.rs`.
  Note the `ValueLayout` one cannot be converted at all — the marker is explicit
  that there are no real fields to name, so it is a
  `CompatibilityClassRequested` violation, not a slot-numbering bug, and fixing
  it means letting the real `ValueLayout.<clinit>` run.
* **Step 4**, making `safe` verdicts checkable rather than asserted. They are
  claims about JDK 25 that nothing in the build re-checks; a JDK upgrade should
  fail a test, not corrupt an object.

## What is wrong

A large amount of CratonVM native and VM-internal code reaches into Java objects
by **slot index** (`heap.set_field(obj, 1, …)`, `heap.get_field(obj, 3)`) rather
than by resolving the field by name. Those indices were chosen against
CratonVM's own fabricated layout for the class. The moment the class is loaded
from real JDK bytes — which is the entire premise of `--jdk-only` — the index
still resolves, still type-checks, and now points at a *different field*.

There is no fault, no exception and no log line. The object is simply wrong
afterwards.

## Evidence

### The registry already maintains a hand-curated list of classes where this happens

`native-api/src/registry.rs` (~4391), the `drop_real_layout_synthetic` field
doc, names **six** class families whose synthetic natives had to be dropped
wholesale in real-JDK mode *because of layout drift alone*. Note the mismatch
inside the doc itself: its opening sentence enumerates *"`java/util/StringJoiner`,
`java/io/StringReader`, `java/util/EnumSet`, `LinkedBlockingDeque`, and
`ScheduledThreadPoolExecutor`"*, while the prose that follows also describes
`Pattern`/`Matcher` and never returns to `ScheduledThreadPoolExecutor`. Treat
the enumeration as incomplete in both directions until someone reconciles it
against `set_drop_real_layout_synthetic`'s actual effect.

* **`java/util/StringJoiner`** — registered with a fake 5-field layout
  (`delim/prefix/suffix/elements-ArrayList/emptyValue`); the real class has 7
  (`prefix/delimiter/suffix/elts[]/size/len/emptyValue`). The synthetic `add`
  reads slot 3 — real `elts`, null — and no-ops, so `size` never moves and
  `toString` renders just prefix+suffix. **A silently empty join, not a crash.**
* **`java/util/EnumSet`** — "the same problem in a more dangerous form": the
  fallback native surface manufactures an abstract `java/util/EnumSet` receiver
  with a two-field synthetic layout, so `EnumSet.of(...)` / `allOf(...)` return
  an empty object with `iterator() == null`.
* **`java/util/concurrent/LinkedBlockingDeque`** — the four-slot fake
  blocking-queue layout leaves real final fields (`lock`, `notEmpty`) null;
  Tomcat's `WriteBuffer.clear()` then fails inside `LinkedBlockingDeque.clear()`.
* **`java/io/StringReader`** — the real class wraps a final `Reader r`; the
  synthetic constructor writes the old `(content, pos, length)` slots, leaving
  `r` null before `mark()` delegates.
* **`java/util/regex/Pattern` / `Matcher`** — the legacy regex natives allocate
  real-layout objects but write the old synthetic slots, leaving fields such as
  `Matcher.locals` uninitialised.

Note what that list is: the classes where the drift was severe enough to be
noticed and worked around. It is a sample, not a census.

### The wave-1 `JDK-ONLY-LAYOUT:` marker sweep

Wave 1 introduced a `// JDK-ONLY-LAYOUT: <verdict>` marker with the verdicts
`safe`, `unknown`, `breaks-under-strict` and `converted`. As of 2026-07-31 there
are **10 markers across 3 files**, distributed as follows (corrected against the
re-landed tree — the original filing put two `safe` verdicts in `vm_object.rs`
where there is one plus a file-level anchor):

| File:line | Verdict | Site |
|---|---|---|
| `vm/src/vm/vm_util.rs:251` | `breaks-under-strict` | `FileInputStream` fallback: writes `Int(1)` into slot 1, which on a real `java/io/FileInputStream` is `path:String`. Flagged "dead arm on real bytes" |
| `vm/src/vm/vm_util.rs:2069` | `breaks-under-strict` | FFM `ValueLayout` preseed: assumes slot 0 = `byteSize` on an object of an **interface** type that has zero instance fields |
| `vm/src/vm/vm_util.rs:1822` | `converted` | `Throwable` cause-chain walk, converted from raw slots to name lookup |
| `vm/src/vm/vm_util.rs:2756`, `:3572` | `safe` | `NormalizerBase$ModeImpl`, `AtomicInteger`, verified against JDK 25 |
| `vm/src/vm/vm_object.rs:28` | `safe` (**file-level anchor**) | the `java/lang/String` slot convention — slots 0..3 = `value`/`coder`/`hash`/`hashIsZero` — asserted to be the *real* JDK 9+ declaration order, not a synthetic invention. Every `String` slot literal in the file inherits this verdict |
| `vm/src/vm/vm_object.rs:689` | `safe` | speculative `String` shape probe; deliberately kept index-based, because a named lookup would resolve `value` off whatever class the receiver actually is and defeat the shape check |
| `vm/src/vm/vm_object.rs:1014` | **`unknown`, ranked HIGH** | class-mirror populator writes an `Int` over slot 0 of a real `java.lang.Class`, which JDK 25 declares as `Constructor<T> cachedConstructor` — a *reference* slot. An **overlay**, not a mis-numbering: the safety claim rests on this VM's reference-vs-primitive decode, not on HotSpot's |
| `vm/src/vm/vm_object.rs:1212` | **`unknown`** | primitive-mirror `Int(-1)` marker over the same slot 0. The marker says to resolve both together, and notes a primitive mirror has no legitimate `cachedConstructor` reader, so it can move to the `primitive_mirrors` side table if the overlay proves destructive |
| `vm/src/vm.rs:34` | `safe` (**whole file**) | ~500 raw slot accesses, all inside `#[cfg(all(test, feature = "synthetic-jdk"))]`. A verdict about *reachability*, not quality: `synthetic-jdk` is a build feature that excludes the real class library, whereas `--jdk-only` is a runtime policy on a real image, so none of it is reachable from a strict run |

The two `breaks-under-strict` sites are worth reading in full; both are exactly
the shape described above. The two `unknown` verdicts are a *different* hazard
and should not be triaged with the same instinct: they are **overlays** — a
VM-internal value written deliberately on top of a real JDK field — where the
question is not "is this the right slot" but "does writing an `Int` where the
image declares a reference corrupt anything". The `Throwable` one is the
clearest illustration of the ordinary failure mode:

> A synthetic `java/lang/Throwable` stub is `instance_fields(2)` — `_f0` =
> message, `_f1` = cause — but the REAL JDK declares `backtrace`,
> `detailMessage`, `cause`, `stackTrace`, `depth`, `suppressedExceptions`, so
> real slot 0 is `backtrace` and the message lives at slot 1. Reading slot 0 as
> the message against real bytes is a silent wrong-field read.

That one was fixed (converted to a name-walking lookup). The others were not.

**10 markers is the count of sites someone got to, not the count of sites that
exist.** The marker sweep covered `vm/src/vm/`; `native-builtins`,
`native-collections` and `native-io` — where the great majority of index-based
field access lives — were not swept.

## Why it was not fixed in wave 1

Wave 1 is measurement, not deletion (contract §10). Two of the three verdicts
are also not mechanical:

* The `ValueLayout` site cannot be "converted to named-field lookup" at all —
  the marker says so explicitly: *"there are no real fields to name. It is a
  `CompatibilityClassRequested` violation, not a slot-numbering bug."* Fixing it
  requires letting the real `ValueLayout.<clinit>` run, which requires
  `jdk/internal/misc/UnsafeConstants` to be backfilled with real platform values
  before class preparation.
* The `safe` verdicts are only safe *against JDK 25*. They are assertions about
  a specific image, and nothing in the build re-checks them.

## What specifically must change

1. **Finish the sweep.** Extend the `JDK-ONLY-LAYOUT:` marker discipline to
   `native-builtins`, `native-collections`, `native-io` and `vm/src/native/`.
   Until that is done, the 10 markers understate the problem by an unknown
   factor.
2. **Adjudicate the two `unknown` verdicts** in `vm/src/vm/vm_object.rs`.
3. Replace `breaks-under-strict` sites with name-resolved field access
   (`find_field_recursive(cid, "fd", &cm.class_store)` — the pattern the
   `FileInputStream` site already uses on its *primary* path, with the raw slot
   only as a fallback), or with a structured refusal under
   `CompatibilityMode::JdkOnly` where there is no real field to name.
4. Make `safe` verdicts checkable rather than asserted: a startup or test-time
   assertion that the named field really is at the assumed index for the loaded
   image, so a JDK upgrade fails a test instead of corrupting an object.

## How to verify a fix

* Per site: load the real class and assert `find_field_recursive(cid, name)`
  returns the index the code assumes. A `safe` claim that cannot be expressed as
  such an assertion is not verified, it is remembered.
* End to end: the five `drop_real_layout_synthetic` classes are the ready-made
  regression corpus. A correct fix should let each of them keep its native
  surface *without* the drop — `StringJoiner.add()` moving `size`,
  `EnumSet.of()` returning a non-null iterator, `LinkedBlockingDeque.clear()`
  surviving Tomcat's `WriteBuffer.clear()`.
* `--dump-class-origins` (contract §5) tells you which classes came from real
  bytes in a given run; any index-based access to a class listed `boot-image` or
  `application-classpath` is by definition suspect.

## Blast radius if done wrong

Converting an index to a name lookup is safe but not free — the resolution walks
the superclass chain and these are hot paths. Converting the *wrong* index
(picking the field the code was accidentally hitting rather than the one it
meant) preserves today's behaviour and silently entrenches the bug.

The dangerous direction is dropping a native surface without checking the real
bytecode is self-contained: `StringJoiner` is documented as safe to drop because
"the real bytecode is self-contained and correct", but that is a per-class
finding, not a general rule.

## Related

* `docs/known-issues/jdk-only/README.md` — index.
* The `StringJoiner` divergence between the two real-protected-stub allow-lists
  is a *separate* consequence of the same class's layout drift; see
  [real-protected-stub allow-lists diverge](../../internal/jdk-only-real-protected-stub-allowlists-FIXED-20260804.md) (reconciled 2026-08-04).
* [`docs/jdk-only-object-layout-audit.md`](../../jdk-only-object-layout-audit.md)
  — the companion audit. The original filing recorded that this file did not
  exist; **it does now**, and it is the right starting point for the sweep in
  step 1. Read it before extending the marker discipline into a new crate, so
  the verdict vocabulary stays consistent.
