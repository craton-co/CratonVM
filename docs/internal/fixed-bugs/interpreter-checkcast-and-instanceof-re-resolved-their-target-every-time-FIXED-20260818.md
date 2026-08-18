# `checkcast` and `instanceof` re-resolved their target class on every execution — FIXED 2026-08-18

**Status:** FIXED. The resolved cast-site cache is default-ON;
`CRATONVM_JIT_NO_CAST_SITE_CACHE=1` opts out.

**Reproducers:** `probes/InterpDecodedOpcodeCostProbe.java` (cost),
`probes/CastSiteCacheTortureProbe.java` (correctness).

## What was wrong

Every executed `checkcast` and `instanceof` did this before it could answer:

```rust
let target_class_name = {
    let cm = shared.classes.class_manager.read();     // RwLock read
    class.constant_pool.get_class_name(*index)
        .to_string()                                   // String allocation
};
…
resolve_class_loader_aware(shared, thread, referencing_class_id, &target_class_name)
…
shared.classes.class_manager.read().is_subclass_of(…)  // second RwLock read
```

A `String` allocation, a full loader-aware resolution **by name**, and two
separate `class_manager` read acquisitions — per execution, for a
`(referencing class, cp index)` pair whose answer is fixed.

This is the same defect the field path had, in the same words. `site_cache`'s
module docs describe that one as re-deriving "the field-owning class from its
name on every access, cache hit included — two `String` allocations, two more
`class_manager` read acquisitions and a full `resolve_class_loader_aware`". The
field, method and `new` sites all got a cache. The cast sites did not.

## The fix

`CastSiteCache = SiteCache<ClassId>` — the existing generic, the existing key,
the existing three-epoch validity condition. Default-ON, opt-out spelled like
the `new` site cache's.

**A hit answers only the resolution.** The assignability test still runs on
every execution. A hit is taken only when the receiver is not an array and only
when `is_subclass_of` says yes; a negative answer falls through to the full
name-based path untouched, because the five fail-open fallbacks after it
(`loader_aware_name_assignable`, `synthetic_implements`,
`proxy_instance_satisfies_target`, `annotation_proxy_satisfies_target`,
`lambda_proxy_satisfies`) are all name-based. So the cache accelerates the
assignable case and leaves every refusal exactly as it was.

### Why it is NOT the existing `new`-site table

`ClassSiteCache` already maps `(referencing class, cp index)` to a resolved
class, so sharing it is the obvious economy. It would be a bug.

The same `CONSTANT_Class` entry can be referenced by a `new` **and** by a
`checkcast` in one class. `ClassSiteCache`'s hit path deliberately skips the
initialization check, on the stated grounds that a fill only happens after
`ensure_class_initialized_shared` returned `Ok`. A `checkcast` performs
resolution but **not** initialization (JVMS §6.5), so a `checkcast` fill cannot
carry that guarantee — and a `new` served from one would allocate an instance of
an uninitialized class.

Separate tables keep each cache's precondition its own.

### GC-safety of the hit path

The original pins the receiver in `native_pin_roots` before the assignability
work, because loader-aware resolution can safepoint. The hit path does not need
that pin and does not take it: `kind_of` and `class_id_of` are object-header
reads and `is_subclass_of` is a read lock over an id table. Nothing on that path
allocates, loads a class or resolves anything, so the receiver cannot move. The
slow path still pins, unchanged.

(`kind_of` rather than `array_descriptor_of` for the array test — the latter
builds a `String` descriptor the test would only discard.)

## Measurements

One binary, one env var, `--nojit`, six runs — **three with the OFF arm first
and three with the ON arm first**, because a single order cannot distinguish a
cache win from a warm-up effect. Marginal ns per opcode:

| | cache OFF | cache ON |
|---|---|---|
| `checkcast` | 634 / 528 / 426 / 573 / 442 / 488 | 141 / 206 / 260 / 209 / 168 / 257 |
| `instanceof` | 569 / 499 / 425 / 503 / 424 / 324 | 122 / 153 / 191 / 193 / 124 / 205 |
| `iadd` (control) | 16.6 / 16.3 / 11.8 / 18.6 / 11.3 / 12.5 | 17.2 / 15.6 / 14.9 / 23.3 / 13.7 / 10.4 |

Medians: **`checkcast` 508 → 207 (2.5x)**, **`instanceof` 461 → 172 (2.7x)**.

The two sets do not overlap for either opcode in either order — `checkcast` ON
peaks at 260 against an OFF floor of 426; `instanceof` ON peaks at 205 against
an OFF floor of 324. The control spans 10-23 ns across the whole matrix without
tracking the arm; this host was running other builds throughout, which is why
the absolute numbers are higher than earlier sessions and why every arm is
interleaved.

## Verification

**Engagement first.** `hit=640410 miss=25 fill=25` on the cost probe under the
default; `hit=0 miss=0 fill=0` under the kill switch. A cache that never fired
would pass every correctness check below for the wrong reason.

`probes/CastSiteCacheTortureProbe.java` is built for the fact that every way
this cache can be wrong is **silent** — a wrong target id makes `instanceof`
answer a plausible boolean and `checkcast` accept or reject the wrong type, and
nothing throws. It covers: the same simple name reached from two different
referencing classes (the half of the key a tag check might drop); more distinct
cast sites than the table has slots, rotated so entries evict each other
continuously; interface, abstract-supertype and negative answers; array
covariance and the primitive-array refusals; `null` under both opcodes; and the
three `ClassCastException`s that must still be thrown.

Output is **identical** across four arms: cache ON, cache OFF, JIT-on, and
HotSpot.

difftest: 0/5 seeds diverged across `jit-on`, `nojit` and `interp-decoded`,
including `InterpCpOpcodeParity`.

## Two instrument corrections made here

**The probe was measuring the wrong thing.** `checkcastKernel` read a field
*through* the cast (`((Holder) o).a`), so its marginal cost was checkcast **plus
a getfield** — the most expensive opcode in the table. Reported as "checkcast",
that inflated it by ~180 ns and made checkcast look 1.7x `instanceof` when the
two are within noise of each other. The confound was written in the probe's own
comments and then not applied when the summary table was read, which is the more
useful half: **an instrument's caveat has to live in the output, not the
source.** The kernel now consumes the cast with an `if_acmpne` (~10 ns,
fast-pathed): 16 `checkcast`, 0 `getfield`.

**A counter name implied the wrong cause.** The first version folded "hit
present but unusable" into `CAST_REJECT_LOADER`. The torture probe reported
`reject=26446` against `fill=34` — which reads as "the cache is being refused
for loader reasons" when what is happening is that the cache is answering and
the answer is `false`. Split into `CAST_REJECT_LOADER` (a fill refused) and
`CAST_UNUSABLE` (a hit whose answer could not be used). A counter whose name
implies the wrong cause is the same defect as one that never fires.

## What this leaves

`tableswitch` and `lookupswitch` are the last two opcodes with no fast-path arm
(48 ns and 36 ns marginal). Their operands are variable-length and 4-byte
aligned, so an arm must parse the padding and table out of the raw bytes rather
than read two operand bytes — viable and allocation-free, but an off-by-one
there is a wild jump, so it deserves its own change and its own verification.

The two runtime-error conversion points in the dispatch loop still disagree by
construction; they were made to agree on the stack-overflow normalization
(2026-08-18) but merging them into one remains the real fix.
