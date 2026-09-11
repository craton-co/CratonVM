# The `Class`-typed local was a mirror the collector had already freed

| | |
|---|---|
| **Status** | **FIXED**, 2026-09-11. Root-caused and closed; the reported failures no longer occur on any arm this page measures. |
| **Fix** | `vm/src/memory/roots.rs` — step 6 defers a class mirror to `mirror_pin` propagation only when the registry actually NAMES that mirror (`mirror_deferral_is_covered`). |
| **Was** | `docs/known-issues/springboot/flyway-aot-receiver-class-confusion-under-concurrency-20260910.md` |
| **Residual** | A DIFFERENT defect keeps `CRATONVM_DBG_GC_STRESS=262144` failing this class. It is not this one — see [Residual](#the-residual-and-why-it-is-not-this-defect). |

## The defect

`memory::roots::collect_roots` step 6 roots every `java.lang.Class` mirror in
`SharedVm::class_mirrors` — except that, when class unloading is live, it DROPS
a mirror whose class was defined by a user-defined loader. The dropped mirror is
supposed to be reached another way: `cratonvm_types::mirror_pin` propagation,
which marks every mirror a live `ClassLoader` defined. That is the edge a real
JVM gets for free from `ClassLoader.classes` and CratonVM's synthetic loader
model does not carry.

**The two halves were keyed off different facts, and could therefore
disagree.**

* The DROP tested `ClassLoaderId::UserDefined` — the permanent classification
  written when the class is registered.
* The REPLACEMENT is written by `vm_object::get_or_create_class_mirror`, and
  only when `classloader::defining_loader_for` already had a recorded pairing
  for that class at the moment the mirror was minted.

A class that is `UserDefined` but has no recorded pairing was deferred to a
propagation with **no row for it**. Nothing rooted it. The collector freed a
`java.lang.Class` that live bytecode was still using, the allocator re-served
the address, and the next read through it returned whatever now lives there.

That is the whole page. Every symptom it carries is one consumer reading one
freed mirror:

| symptom | what read the freed mirror |
|---|---|
| `NoSuchMethodError: 'boolean java.lang.String.isAssignableFrom(java.lang.Class)'` | `ClassUtils.isAssignable`'s `lhsType`, from `ResolvableType$1.val$clazz` |
| `NoSuchMethodError: 'boolean ...ScopedProxyMode.isAssignableFrom(java.lang.Class)'` | the same field, a different re-serving |
| `NoSuchMethodError: 'boolean java.util.LinkedHashMap$Entry.isPrimitive()'` | the same local at the NEXT bytecode, pc 22 |
| `AbstractMethodError: ...AnnotatedElement.getDeclaredAnnotations() has no Code attribute` | `EventListenerMethodProcessor.isSpringContainerClass(Class)`'s argument |
| `NoSuchMethodError: 'java.util.HashMap$Node.getConstructor([Ljava/lang/Class;)'` | `LogFactory.newStandardFactory` pc 140, the `Class.forName` result on the operand stack |

The fifth is the one that names the mechanism out loud. `CRATONVM_DBG_CCE_BT`'s
dispatch dump reports the receiver as

```text
NSME-RECV addr=0x7ddfdcd8c538 ... epoch=2
NSME-RECV SHAPE kind=Object num_fields=4 mirror_of=org/apache/commons/logging/impl/Slf4jLogFactory
                [0]=Int(44961) [1]=Object(...) [2]=Object(...) [3]=Object(...)
```

The mirror registry still says that address is `Slf4jLogFactory`'s mirror. The
object AT the address has four fields and an `Int` hash: it is a
`java.util.HashMap$Node`. The mirror did not move — it was **reclaimed**, and
the address handed out again. A heap field (`ResolvableType$1.val$clazz`) can
only hold a re-served address if the object it named died; a relocation would
have rewritten the field.

## The measurement that settles it

`CRATONVM_DBG=mirrorpin` prints, per collection, every mirror step 6 defers and
every mirror `reconcile_class_mirrors` finds unmarked. One process per run,
`module/spring-boot-flyway ...
ResourceProviderCustomizerBeanRegistrationAotProcessorTests`, with
`CRATONVM_NO_JIT_INLINE_TLAB_NEW=1` (see [the forcing
function](#the-forcing-function)), three runs per binary:

| binary | mirrors DEFERRED | mirrors ROOTED | mirrors reconciled `is_marked=false` |
|---|---:|---:|---:|
| `dev` (before) | 1263 / 1263 / 1263 | 0 / 0 / 0 | **754 / 767 / 754** |
| `dev` + this fix | 5 / 5 / 5 | 1258 / 1258 / 1258 | **0 / 0 / 0** |

Seven hundred and fifty-odd `java.lang.Class` objects freed while the program
was using them, on every run, **including runs whose test PASSED** — a freed
mirror only becomes a visible failure once the allocator re-serves its address
before the next read. That is why this row read as flaky: the census is the
defect, the test outcome is a sampling of it.

The `after` row is the split the rule now draws, and it is the whole change: the
five mirrors `mirror_pin` actually names stay DEFERRED — so class unloading keeps
exactly the window it is entitled to — and the 1258 it does not name are ROOTED
instead of being handed to a propagation that has no row for them.

## The same conclusion from the other end

Before the fix, turning the whole deferral off — `CRATONVM_LOADER_UNLOAD=0`,
which makes `conditional_loader_metadata` return `false` and roots every mirror
unconditionally — clears the row. One burst, both arms in every round:

| arm | failed / runs |
|---|---:|
| forcing arm, deferral on (control) | **9 / 12** |
| forcing arm, `CRATONVM_LOADER_UNLOAD=0` | **0 / 12** |

One-sided Fisher p ~ 4e-5. That localisation is what pointed at step 6; the
census above is what said WHICH mirrors and why.

## The forcing function

`CRATONVM_NO_JIT_INLINE_TLAB_NEW=1`. It sends every compiled `new` through the
allocation helper instead of the inline TLAB bump, which changes how often a
collection lands inside the window, and it raises this row's rate without
changing the defect: the failures it produces are the same signatures, and the
mirror census is the same 750-odd either way.

**It is a rate lever, not the cause.** So is the JIT itself, and so is
concurrency: the original page's `--nojit` 0/40 and its "only when several JVMs
run at once" are both this — more allocation, more collections, more chances for
a freed mirror's address to be re-served before it is read again. Nothing in the
fix touches the compiler.

## Verification

Paired bursts, every arm in EVERY round, on Azure Linux (`20.80.105.49`),
`dev`@`8f166641` and the same tree plus this change. Both binaries run side by
side in the same round, so host load cannot be the arm. Two independent bursts,
40 runs per arm each, 8 processes concurrent:

| burst | arm | failed / runs |
|---|---|---:|
| A | before, forcing | 3 / 40 |
| | **after, forcing** | **0 / 40** |
| | before, natural | 1 / 40 |
| | **after, natural** | **0 / 40** |
| B | before, forcing | 4 / 40 |
| | **after, forcing** | **0 / 40** |
| | before, natural | 1 / 40 |
| | **after, natural** | **0 / 40** |

Pooled: **9 failures / 160 runs before, 0 / 160 after** (one-sided Fisher
p ~ 0.002). The signatures on the `before` arms are this page's own:
`String.isAssignableFrom(Class)`, `LinkedHashMap$Entry.isPrimitive()`, and six
`AnnotatedElement.getDeclaredAnnotations() has no Code attribute`.

Read the census above first, though. At this rate a burst can only ever be
suggestive — which is exactly how this row absorbed two sessions — and the
census is the same statement without the sampling: 750-odd freed mirrors per
run, every run, versus zero.

**Class unloading is unaffected**, which is the thing this fix could plausibly
have broken. `vm/tests/resources/class_loader_unload/LoaderUnloadProbe` on the
default collector:

| binary | result |
|---|---|
| before | `liveLoaders=0 liveClasses=0 unloadedDelta=6 ok=true` |
| after | `liveLoaders=0 liveClasses=0 unloadedDelta=6 ok=true` |

A blunter fix does not survive that gate, and the attempt is recorded here so it
is not re-derived: gating the ZGC arm of `conditional_loader_metadata` on
`class_unload_marking()` (G1's rule) also clears the row — 0/10 against 4/10 —
but it takes `ok=true` to `liveLoaders=6 ok=false`, because `System.gc()`'s
collection is not a class-unload mark. The rule that survives both is the one
that shipped: defer only what the propagation actually names.

## The residual, and why it is not this defect

`CRATONVM_DBG_GC_STRESS=262144` still fails this class, on both arms, with a
signature this page never carried:

```text
java.lang.ExceptionInInitializerError
  at ...MessageSourceSupport.<clinit>(MessageSourceSupport.java:48)
Caused by: java.lang.ClassCastException: class java.lang.Object cannot be cast
           to class [Ljava.lang.Object;
  at java.util.Spliterators.spliterator(Spliterators.java:130)
  at java.util.Arrays$ArrayList.spliterator(Arrays.java:4257)
```

An `Arrays$ArrayList`'s backing-array field reading back as a plain `Object` is
the `bindabletests-stale-objectref-family-across-allocation` shape, not a class
mirror, and it is present before and after this fix. It is also GC-stress-only:
no ordinary run of this class, at any concurrency measured here, produces it.
Filed against that family, not this row.

## What the original page got right, and what it cost

The **Method** section — every arm in one burst, with the control in it — is
what made every number above readable, and the page's own correction of its
"it reproduces alone" claim is what kept a load artefact from being reported as
a localisation a second time.

What cost this row two sessions is where it looked. `--nojit` is clean, so the
page concluded "codegen", and then deny-swept the four Spring classes in the
stack. Those were all correct observations about a lever and all false about the
site: the bad value is not manufactured anywhere in that stack, and the page
says so — "the wrong value is already in the argument by the time
`isAssignable` is entered". That sentence is the finding. The next question was
not "which compiled body writes it" but "what freed the object it names", and
the receiver dump answers that in one run: `mirror_of=` naming a registry entry
whose address now holds a `HashMap$Node`.

## How to reproduce the defect on an unfixed binary

```bash
CRATONVM_NO_JIT_INLINE_TLAB_NEW=1 CRATONVM_DBG=mirrorpin \
  bash /data/sbone.sh module/spring-boot-flyway \
  org.springframework.boot.flyway.autoconfigure.ResourceProviderCustomizerBeanRegistrationAotProcessorTests \
  <binary> 2>&1 | grep -c 'is_marked=false'
```

Any non-zero count is this defect: that many `java.lang.Class` objects were
freed while still in use. A fixed binary reports `0`. The count is deterministic
enough to A/B on a single run per arm, which is what makes this row cheap to
re-check — the pass/fail of the test itself is not.
