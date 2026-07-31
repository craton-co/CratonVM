# Fabricated object layouts leak into native code — index-based field access breaks silently when the class becomes real

**Status:** OPEN — JDK-only wave-2 work item, filed 2026-07-31. **DANGEROUS:
every instance is a silent wrong-field read or write, never an exception.**

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

`native-api/src/registry.rs`, the `drop_real_layout_synthetic` field doc, names
five classes whose synthetic natives had to be dropped wholesale in real-JDK
mode *because of layout drift alone*:

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

Note what that list is: five classes where the drift was severe enough to be
noticed and worked around. It is a sample, not a census.

### The wave-1 `JDK-ONLY-LAYOUT:` marker sweep

Wave 1 introduced a `// JDK-ONLY-LAYOUT: <verdict>` marker with three verdicts —
`safe`, `unknown`, `breaks-under-strict`. As of 2026-07-31 there are **10
markers across 3 files**:

| File | Verdict | Site |
|---|---|---|
| `vm/src/vm/vm_util.rs` | `breaks-under-strict` | `FileInputStream` fallback: writes `Int(1)` into slot 1, which on a real `java/io/FileInputStream` is `path:String` |
| `vm/src/vm/vm_util.rs` | `breaks-under-strict` | FFM `ValueLayout` preseed: writes slots 0/1 on an object of an **interface** type that has zero instance fields |
| `vm/src/vm/vm_util.rs` | `converted` | `Throwable` cause-chain walk, converted from raw slots to name lookup |
| `vm/src/vm/vm_util.rs` ×2, `vm/src/vm.rs` ×1 | `safe` | `NormalizerBase$ModeImpl`, `AtomicInteger`, verified against JDK 25 |
| `vm/src/vm/vm_object.rs` ×2 | **`unknown`** | not yet adjudicated |
| `vm/src/vm/vm_object.rs` ×2 | `safe` | — |

The two `breaks-under-strict` sites are worth reading in full; both are exactly
the shape described above. The `Throwable` one is the clearest illustration of
the failure mode:

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
  [real-protected-stub allow-lists diverge](real-protected-stub-allowlists-diverge.md).
* A `docs/jdk-only-object-layout-audit.md` was expected to accompany this
  finding; **it does not exist in the tree as of 2026-07-31** and this record
  therefore does not link to it.
