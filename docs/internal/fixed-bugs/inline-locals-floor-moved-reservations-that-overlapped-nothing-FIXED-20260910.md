# The open-inline-locals floor moved reservations that overlapped nothing

**Status: FIXED 2026-09-10.** Closes
`module/spring-boot-kafka … KafkaAutoConfigurationIntegrationTests`, the last
CratonVM-only Spring Boot failure in the 2026-09-10 six, and retires
`internal/fixed-suite-bugs/springboot/kafka-scala-statics-anyhash-jit-miscompile-FIXED-20260910.md`.

| arm | before | after |
|---|---|---|
| `AnyHashOnly` reproducer, 300,000 calls, 10 runs | **9 runs ≈299,400 wrong**, 1 clean | **10 runs `bad=0`** |
| `KafkaAutoConfigurationIntegrationTests`, 3 runs | 3 tests, **2 failed** each time | 3 tests, **0 failed**, 3 runs of 3 |
| `inline_locals_floor_bumps` on that run | 1 | 0 |

## What the defect was

`open_inline_locals_floor` (added 2026-09-07, and correct about the hazard it
was written for) returns the TOP of the highest OPEN ENCLOSING inline scope's
locals. `reserve_spill_slots` then applied it as

```rust
if start < floor { start = floor; }
```

which is a **one-sided** test. It asks where the reservation BEGINS relative to
the top of those locals, and never asks how far it extends. A reservation that
would have fitted entirely UNDERNEATH an enclosing splice's locals — owning no
word that scope owns, overlapping nothing — was displaced past them just the
same.

The guard's own doc comment says the right thing throughout: *"never start
INSIDE the locals of an inline scope that is still open"*, *"the lowest offset a
reservation may start at without handing out a word an OPEN inline scope's
LOCALS still own"*. Neither sentence licenses moving a reservation that owns no
such word. The code was stricter than its specification, and the extra
strictness was not conservative — it was a miscompile.

## The measurement

One JIT-compiled Scala method, no Kafka and no Spring:

```java
Long v = Long.valueOf(-2L);
for (int i = 0; i < iters; i++) {
  int h = Statics.anyHash(v);      // must be -2 for every i
  if (h != -2) bad++;
}
```

`scala.runtime.Statics.anyHash` splices `anyHashNumber`, which splices
`longHash` — a two-deep nest, which is what it takes to have an ENCLOSING scope
open at all.

The whole 300,000-call run took **one** floor bump, and that one bump was
enough to make the compiled body answer a **per-run constant** for every
argument: three distinct `Long`s returned the same wrong `int`, and `Double`,
`Float`, `Integer` and `String` receivers stayed correct in the same loop.

That the bump was gratuitous is measurable rather than inferred. The same run
with `CRATONVM_JIT_NO_INLINE_LOCALS_FLOOR=1` and `CRATONVM_DBG=jit-slot-overlap`
reports **zero ENCLOSING overlap rows** — so with the floor removed, the
reservation it had been moving overlapped no scope's locals at all. The new
`CRATONVM_DBG=jit-locals-floor` names it outright:

```text
[jit-locals-floor] KEPT Push cursor=56 slots=1 start=56 one_sided_floor=88
                   scopes=2 in scala/runtime/Statics.anyHashNumber:(Ljava/lang/Number;)I
```

One word at `56..64`; the enclosing scope's locals end at `88`. Twenty-four
bytes of clearance, and the old rule moved it anyway.

The A/B is interleaved, one binary, one switch:

| pair | default | `NO_INLINE_LOCALS_FLOOR=1` |
|---|---|---|
| 1–10 | 299462 · 296697 · 299498 · 298959 · **0** · 299420 · 299098 · 299498 · 298824 · 298809 | 0 × 10 |

## The fix

`inline_locals_clear_of_range(start, slots)` replaces the one-sided comparison
with the RANGE test — the same `start < s_end && s_start < end` that
`dbg_note_spill_overlap` already uses to decide whether a report is a hazard.
It iterates, because clearing one scope can walk the range into the next one
up; each pass clears at least one scope and never lowers the cursor, so the
loop is bounded by the splice depth.

Everything the 2026-09-07 fix was written for still fires. That fix's census
(`CriteriaWindowFunctionTest`, 151 of 304 overlap reports against an enclosing
scope) recorded that *"in every sampled report the reservation range was
EXACTLY the scope's locals range"* — an exact range intersects under either
rule. Its regression test,
`a_reservation_never_starts_inside_an_open_inline_scopes_locals`, passes
unchanged, including its three-deep shape and its assertion that the cursor
comes back above the enclosing locals.

## Why the new test asserts a NON-movement

`a_reservation_that_fits_under_an_open_scopes_locals_is_not_moved` is the other
half of the contract, and it is the half a guard's own tests usually cannot
see: every existing test asks whether the floor MOVES what it should, and
restoring the one-sided rule passes all of them. It is also the change any
future overlap report will suggest, because it looks strictly more
conservative.

So the test pins its own premise first — `open_inline_locals_floor() ==
enclosing_top` and `cursor < enclosing_top`, i.e. *the one-sided rule would
have moved this one* — and only then asserts that the reservation did not move.
Without that premise the test would pass against either rule and prove nothing.

## The instrument

`CRATONVM_DBG=jit-locals-floor` prints one line per reservation the floor
considers while a splice is nested, and prints BOTH outcomes:

* `BUMPED` — the guard doing its job.
* `KEPT` — a reservation the one-sided rule would have moved and the range rule
  left alone. This is the population that was the miscompile, so a run can
  COUNT it rather than infer it from a bug report.

`inline_locals_floor_bumps()` (in `jit-method-stats`) counts only the first
kind, which is why it reads `1` before this fix and `0` after on the same
workload.

## What this does NOT close

`module/spring-boot-flyway … ResourceProviderCustomizerBeanRegistrationAotProcessorTests`
was filed the same day with a note that it might be the same underlying fault.
It is not, and that is measured rather than assumed: four arms in one burst,
24 runs each, gave **21 of 24 failed on the pre-fix binary and 21 of 24 on the
post-fix binary** -- the same number -- against 0 of 16 with `--nojit`. A
smaller 16-run burst had read 11 against 15 and looked like this fix had made
that vector worse; it had not, and the larger sample is the one to cite.
Directly: on a failing Flyway run the floor logs **zero** ENCLOSING overlaps and
names no frame in that stack. Its page has been re-measured rather than
retired. See
`known-issues/springboot/flyway-aot-receiver-class-confusion-under-concurrency-20260910.md`.

## Related

* `internal/fixed-bugs/inline-splice-return-value-lands-on-an-enclosing-callees-live-local-FIXED-20260907.md`
  — the fix this one narrows, and the source of the floor.
* The dead end recorded on the Kafka page is worth keeping in view: a
  single-run sweep of eighteen `CRATONVM_JIT_*` flags appeared to isolate
  `CRATONVM_JIT_CALL_SPILL_ELISION` at 940x, and five runs per mode showed every
  mode spanning the same range. This vector is bimodal; measure it with at
  least five runs per arm. The arm that finally answered
  (`NO_INLINE_LOCALS_FLOOR`) did so 10 times out of 10, interleaved, which is
  the standard the earlier candidate failed.
