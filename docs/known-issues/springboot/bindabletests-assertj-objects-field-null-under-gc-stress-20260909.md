# One BindableTests method fails an AssertJ NPE at `CRATONVM_DBG_GC_STRESS <= 262144`, and every stale-reference probe is silent

| | |
|---|---|
| **Status** | OPEN, filed 2026-09-09. Deterministic. Not root-caused. **A different symptom from the crash this class used to take** — see [What changed](#what-changed). |
| **Scope** | `--XX:UseGc Generational`, `CRATONVM_DBG_GC_STRESS <= 262144`. PASSES at 393 216, 524 288, every higher threshold, and unset. |
| **Reproducer** | `org.springframework.boot.context.properties.bind.BindableTests`, Linux x86-64, ~60 s |
| **Result** | `SBRUNNER_RESULT tests=27 failed=1` — one failed assertion, not a VM crash |
| **Was** | the residual of the BindableTests stale-reference family, ten defects of which are fixed — see [the internal page](../../internal/springboot/bindabletests-stale-objectref-family-across-allocation-20260909.md). |

## The symptom

```text
java.lang.NullPointerException: Cannot invoke
  "org.assertj.core.internal.Objects.assertEqual(org.assertj.core.api.AssertionInfo, Object, Object)"
  because "this.objects" is null
```

`AbstractAssert.objects` is assigned once, in AssertJ's constructor
(`this.objects = Objects.instance()`), and read on every assertion. Reading it
as null means either the `putfield` was lost or the `getfield` produced null.
Which of the two has not been established.

## What changed

This class used to CRASH here — a stale reference reaching `invokevirtual` with
an all-zero header, at moving cycle 2 398. That was a family of ten `ObjectRef`s
held across an allocation, all fixed on 2026-09-09. What is left is a different
shape and needs to be treated as a new investigation, not as the same bug moving
around:

| | before | now |
|---|---|---|
| failure | VM crash, stale receiver | one failed JUnit assertion |
| passes at | `>= 524288` | `>= 393216` |
| needs `CRATONVM_GC_RESERVE=0` to be reportable | yes | no |
| stale-reference probes | 12 pin hits, 8 arg, 8 push, 2 native returns | **all zero** |

## Ruled out

Each of these is a measurement on this exact workload, on the current binary.

* **Not a stale reference anywhere the instruments reach.** Every arm of
  `CRATONVM_DBG_DEADREF_STORE` reads zero, in BOTH `--nojit` and JIT runs:
  `[deadref-store]`, `[deadref-pin]`, `[deadref-arg]`, `[deadref-local]`,
  `[deadref-push]`, `[deadref-nret]`, `[deadref-capture]`, `[deadref-singleton]`.
* **Not the remembered set.** `CRATONVM_GC_VERIFY_RSET=1` reports **zero**
  missing old→young edges. (The `java/lang/Module slot=0` edge that dominated
  earlier pages was `build_module` writing an unpinned `layer`, now fixed.)
* **Not a dangling heap field.** `CRATONVM_DBG_HEAP_STALE=1` reports nothing.
* **Not the TLAB allocator.** The three full-span `[tlab-audit]` tripwires — a
  carve handing out occupied memory, a filler burying live objects, a bump
  running through a retired chunk — all read zero.
* **Not the JIT.** `--nojit` fails identically.
* **Not the descriptor-coercion guard nulling this field.** That guard does null
  a reference read whose value contradicts the slot's descriptor, and it fires
  3 038 times on this run — but every site is a VM-internal mirror read
  (`mirror_class_id`, `native_class_get_name`,
  `native_class_is_assignable_from`, `sb_view`, `props_defaults`, …), none of
  them `AbstractAssert.objects`. Those are worth their own page; they are not
  this.

## Where to start

Decide between the two readings first, because they have nothing in common:

1. **The write was lost.** Watch the field cell across the constructor —
   `CRATONVM_DBG_WATCH_ADDR=<cell address>` narrates every allocator and
   collector event that touches an address, dated by collection.
2. **The read produced null.** `getfield` on a live object returning null with
   no coercion-loss report is a decode question, not a GC one.

`Objects.instance()` is a static singleton, so a third possibility is that the
static itself reads null at construction time and the field is faithfully
storing it. That is the cheapest of the three to check and should go first.

## Repro

```bash
CRATONVM_DBG_GC_STRESS=262144 \
pwsh -NoProfile -Command "& '<repo>/apps/spring-boot-suite-runner/run-spring-boot-suite.ps1' \
  -Exe <cratonvm> -JdkHome /data/jdkimages/jdk25-linux/jdk-25.0.4+7 \
  -ClassList <core/spring-boot BindableTests> -Parallel 1 -TimeoutSec 1800 \
  -CratonArgs @('--XX:UseGc','Generational')"
```

No diagnostic lever is needed to reproduce it any more — that is itself part of
the change from the previous defect, which only became reportable under
`CRATONVM_GC_RESERVE=0`.

**Do not run this with `CRATONVM_DBG_HEAP_STALE=1` and expect the same failure**:
that verifier's own walk is documented as fragile (it advances purely by
`gen_object_total_size`) and turns this run into a crash at ~7 s. Its zero
finding above was read from the reports it did emit before that point.
