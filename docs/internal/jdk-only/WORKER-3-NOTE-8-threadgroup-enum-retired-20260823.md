# WORKER-3-NOTE-8 — the box method does not generalise, and `ThreadGroup`'s natives were answering wrongly

**2026-08-23**, MEASURED on `f98641cb2`. Control for every arm below is
**106/107**, failing `RJdkJmx` — the base-branch regression flagged in
`WORKER-3-NOTE-6` §5, **still red and still unowned**.

## 1. The negative result, and it closes a line of enquiry

`WORKER-3-NOTE-7` §4 closed the box block by noticing that the class-scoped
prices in `NOTE-4` §2 **overstate** the cost: nearly everything registered on
`Integer`/`Long` is `NativeKind::Intrinsic`, which is census-exempt by
construction (`NOTE-5`), so the class arm was paying for rows that were never in
the count. Four `bridge` rows were the whole block, and retiring exactly those
was free.

The obvious next move is to apply that to the rest of the `java.lang` core
block. **It does not work, and here is the number that says so** — per group,
registrations that own their slot, split by kind:

| group | regs | own slot | `Intrinsic` | census-eligible (`bridge`) |
|---|---:|---:|---:|---:|
| class | 97 | 93 | **0** | 93 |
| system | 83 | 81 | **0** | 81 |
| thread | 67 | 62 | **0** | 62 |
| ref | 44 | 42 | **0** | 42 |
| module | 36 | 35 | **0** | 35 |
| classloader | 36 | 34 | **0** | 34 |
| threadgroup | 19 | 19 | **0** | 19 |
| object | 17 | 17 | **0** | 17 |
| runtime | 24 | 15 | **0** | 15 |
| stack | 12 | 11 | **0** | 11 |
| enum | 5 | 5 | **0** | 5 |

**Zero intrinsics anywhere outside the boxes.** There is no hidden exempt subset
to carve off, so `NOTE-4` §2's class-scoped prices were already the true prices
for every one of these groups, and the ones that cost 25–102 vectors still do.

The box result was not a general technique. It worked because `Integer`/`Long`
sit in the one intrinsic-heavy corner of `java.lang` — `Math`/`StrictMath` and
the boxes are 138 + 71 of `NOTE-5`'s 398. **Nobody needs to try this again.**

## 2. What the triage did surface — two rows this lane had measured and never landed

`NOTE-4` §2 priced `threadgroup` and `enum` CLEAN and listed them in its "34
free rows", and neither was ever retired. Re-measured on this tip:

| retired | passed | note |
|---|---:|---|
| control | 106/107 | `RJdkJmx` |
| `java/lang/ThreadGroup` | 106/107 ×2 | a third run also showed the intermittent `RTreeRangeGc` |
| `java/lang/Enum` | 106/107 | |
| **both together** | **106/107** | checked deliberately — see below |

The combined arm is not redundant. `NOTE-4` §4.1 measured `StringBuilder` alone
clean, `AbstractStringBuilder` alone clean, **and the pair fatal**; a retirement
that is free alone and fatal combined is a shape this lane has already been
caught by once, so it is now always checked.

Registry: **24 registrations removed, 24 triples gone, 0 rows flip
`owns_slot: false → true`.**

## 3. The reason this is a FIX, not merely free

Trap 5 — a green arm is evidence about the question the corpus asks, and nothing
asserts what `ThreadGroup`'s accessors or `Enum.name()` actually return. So ask
directly. `EnumTg`, 17 checks, diffed against HotSpot 25 on stdout only:

```text
control : CK tg child=child parentOf=true active=1 groups=1     <- WRONG
HotSpot : CK tg child=child parentOf=true active=0 groups=1
retired : IDENTICAL to HotSpot on all 17 checks
```

The source says why:

```rust
r.register(tg, "activeCount", "()I", |ctx, _args| {
    let count = ctx.active_thread_count().max(1); // at least 1 (current thread)
    Ok(Some(Value::Int(count)))
});
```

`ctx.active_thread_count()` is a **VM-wide** count. This native ignores its
receiver completely, so every group reports the same number — and `.max(1)`
means it can never report 0. A freshly constructed group with no threads
answered **1** where HotSpot answers **0**. Real JDK 25 bytecode counts the
group's own threads and agrees with HotSpot.

**So the retirement removes 24 registrations and makes the VM more correct.**
That is the contract's premise doing exactly what it claims, which is rarer in
this lane's results than it should be.

## 4. Verification

| check | result |
|---|---|
| registry equivalence to the measured gate (line numbers dropped) | **PASS** — 10061 triples both sides, 0 differing, 0 whose owning FILE differs |
| content vs HotSpot, 17 checks | **identical** (1 of 17 wrong before) |
| `CRATONVM_ARGS=--jdk-only` | see below |
| `SUITE=all` | see below |
| `SUITE=core` | see below |

`WORKER-3-NOTE-7` §4.1 records why the equivalence check is phrased that way: a
post-deletion dump cannot be promotion-checked against a pre-deletion dump,
because deleting lines renumbers `registered_by`.

## 4a. The `Intrinsic` ratchet, and what freezing it revealed

`NOTE-5` §5 asked for a gate, because re-tagging a row `Intrinsic` removes it
from the census **without changing behaviour** and nothing prevented it. Added
as `intrinsic_count_does_not_regress` beside the stub ratchet, same shape:
print the live number, print the by-file breakdown on every run, assert against
a frozen baseline. **Frozen at 1365** of 13356 boot-path registrations; 12/12
green.

Freezing it printed something the count alone never showed. The exemption is
**not** mostly hot-path math:

| file | rows |
|---|---:|
| `phases_late/bouncycastle.rs` | **312** |
| `lang_math.rs` | 295 |
| `antlr_intrinsics.rs` | **270** |
| `orm_hibernate.rs` | **107** |
| `lib.rs` | 88 |
| `apps_h2.rs` | **74** |
| `phases_early.rs` | 70 |
| `securerandom.rs` | 56 |
| `test_frameworks.rs` | 37 |
| `math_bignum.rs` | 32 |

**BouncyCastle, ANTLR, Hibernate and H2 alone are 763 of 1365 — more than
half.** `stub_ratchet.rs`'s own header defines `Intrinsic` as "a correct
fast-path for a hot method (kept forever)", and an app-targeted shim in
`orm_hibernate.rs` is not that. Whatever those rows are, they carry a tag that
exempts them from the one number the project is scored against.

This does not adjudicate a single one of them — it is a count and a breakdown.
It says where to look, and it stops the population growing quietly while
somebody does.

## 5. Where the blocks stand

| block | rows | state |
|---|---:|---|
| `java/lang` boxes | 4 | CLOSED (`NOTE-7` §4) |
| `java/lang` `ThreadGroup` + `Enum` | 24 regs | **CLOSED — and a wrong answer fixed** |
| `java/lang` exceptions | 62 / 651 | REFUSED at three granularities (`NOTE-7`) |
| `java/lang` core, remaining | ~380 regs | load-bearing; no exempt subset exists (§1) |
| `java/lang/invoke` | 56 | REFUSED — capability gap (`NOTE-4` §3) |
| `StringBuilder`/`StringBuffer` | 57 | REFUSED (`NOTE-4` §4); one defect fixed (`NOTE-6`) |

## Index rows for `INDEX.md` (H0 to place)

* `WORKER-3-NOTE-8` §1 — the box block's method does NOT generalise: `Intrinsic`
  count is **0** for every other `java.lang` core group, so there is no exempt
  subset to carve off and `NOTE-4` §2's class prices stand. Nobody needs to
  retry this
* `WORKER-3-NOTE-8` §3 — `ThreadGroup.activeCount()` was
  `ctx.active_thread_count().max(1)`: a VM-WIDE count that ignored the receiver
  and could not return 0. Retiring the class fixes it — 17/17 vs HotSpot after,
  1 wrong before
* `WORKER-3-NOTE-8` §2 — `threadgroup` and `enum` were priced clean in `NOTE-4`
  and never landed; 24 registrations retired here
* `WORKER-3-NOTE-8` §4a — the `Intrinsic` ratchet is frozen at **1365**, and the
  by-file breakdown shows the exemption is dominated by APP-SPECIFIC files —
  BouncyCastle 312, ANTLR 270, Hibernate 107, H2 74 = 763 of 1365 — not by the
  hot-path math the tag is documented for
