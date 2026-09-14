# WORKER-3-NOTE-7 — the exception retirement is refused at three granularities; the box block is closed

**2026-08-22**, all figures MEASURED on `27c25efda`. This executes the
four-step work-order `WORKER-3-NOTE-4` §5 left behind, and closes
`H14-2`'s `java/lang` box block.

**Control for every row below: 105/107**, failing `RTreeRangeGc` and `RJdkJmx`.
Both are pre-existing and characterised in `WORKER-3-NOTE-6` §5 — `RJdkJmx` is a
regression on the base branch, `RTreeRangeGc` is the open GC record and is
intermittent. **Read every result against 105/107, not against 107/107.**

## 1. Step 1 — the gate now takes triples, and that exposed a flaw in it

`W3_RETIRE_FILE` takes a file of `class` or `class#method#descriptor` lines.

**It is still not a site gate, and the difference matters.** Gating the 62
`getMessage`/`toString` triples the `lib.rs` extras loop registers removed
**122 registrations, not 62** — because a triple gate matches
`(class, name, descriptor)` from **every** registrar, and `lang_misc.rs`
registers the same 62 triples. The `-122` is the only reason this was caught.

So the work-order's step 2, "price the `lib.rs` loop alone", **is still not
expressible** by this instrument. A true site gate needs
`Location::caller()` at `register()` compared against `file:line`. Recorded as
the remaining gap; it did not block the verdict below, because the registry dump
answers the question directly.

## 2. Step 3 — the promotion check, and it FAILS for the loop

`registered_by` is `file:line` from `#[track_caller]`, so a single
`registry.register(...)` call site is one exact line. The 651 registrations on
the `java/lang` exception classes resolve to:

| rows | own slot | site | methods |
|---:|---:|---|---|
| 108 | 103 | `lang_misc.rs:3041` | `<init>` |
| 39 | **9** | `lang_misc.rs:3120` | `getMessage` |
| 39 | **9** | `lang_misc.rs:3156` | `toString` |
| 39 | 39 | `lang_misc.rs:3128` | `getLocalizedMessage` |
| … | | | (9 more `lang_misc` sites, all 39/39) |
| **31** | **31** | **`lib.rs:40370`** | **`getMessage`** |
| **31** | **31** | **`lib.rs:40376`** | **`toString`** |

`lang_misc`'s `getMessage` and `toString` rows are the only two of its thirteen
sites that do **not** own their slots — 9 of 39 each. The `lib.rs` extras loop
is what buries them, which is exactly what that function's own header says it
does ("these 212 rows were the ones actually answering… and the JDK-derived
table was being buried").

MEASURED on the 62 triples: **60 registrations from other sites** would take the
slot — 30 at `lang_misc.rs:3120`, 30 at `lang_misc.rs:3156`.

> **VERDICT: step 3 FAILS.** Deleting the `lib.rs` extras loop promotes **60**
> `owns_slot: false` bodies into service. `H22` nearly shipped that at a scale
> of **16**. The pass condition was never a green suite and this is why.

## 3. Step 4 — and the other two routes are not free either

| what is retired | rows | passed | vs control |
|---|---:|---:|---|
| control | — | 105/107 | — |
| the `lib.rs` loop alone | 62 | *not run* | **refused at step 3, 60 promotions** |
| both sites (62 triples, 122 registrations) | 62 | **104/107** | **−2**: `ROptionalClassForName`, `RJdkFailure` |
| all 41 `java/lang` exception/error classes | 651 | **104/107** | **−2**, the same two |

**All three routes are refused.** The earlier class-scoped 107/107 in
`WORKER-3-NOTE-4` §2 was measured on a different tip against a 21-class set
derived from the shadow population; the 41-class set derived from the
registrations is not free. That earlier row should be read as superseded.

Worth noting for whoever picks this up: retiring **62** triples and retiring
**651** registrations cost the *same* two vectors. The extra 589 registrations —
every `<init>`, `printStackTrace`, `getCause`, `getStackTrace` — are free. The
price is entirely in `getMessage`/`toString`.

## 4. The box block — CLOSED, 4 rows

`H14-2` prices `java/lang` boxes at 4 rows, and `WORKER-3-NOTE-4` §2 measured
the class-scoped retirement at 106/107 with `RJdkReflBox` failing. Both numbers
are right and they are about different things.

**Everything registered on `Integer`/`Long` is `NativeKind::Intrinsic` except
four bridges.** Intrinsics are census-exempt by construction
(`WORKER-3-NOTE-5`), so those four bridges **are** the four rows:

```text
java/lang/Integer.toBinaryString(I)Ljava/lang/String;              lib.rs:11408
java/lang/Integer.toOctalString(I)Ljava/lang/String;               lib.rs:11394
java/lang/Integer.valueOf(Ljava/lang/String;)Ljava/lang/Integer;   lib.rs:9206
java/lang/Long.toHexString(J)Ljava/lang/String;                    lib.rs:11472
```

| retired | passed | new failures |
|---|---:|---|
| `java/lang/Integer` (class) | 105/107 | `RJdkReflBox` |
| `java/lang/Long` (class) | 105/107 | `RJdkReflBox` |
| both classes | 105/107 | `RJdkReflBox` |
| **the 4 bridge triples** | **106/107** | **none** |

`RJdkReflBox` asserts **reflective boxing identity** — whether a reflective
primitive read hands back the canonical cached wrapper, observable only with
`==`; its own header says a change making every reflective path canonical and
its exact inverse both passed the whole suite until it landed. The intrinsic
`valueOf(I)` backs the cache it checks. So a class-scoped retirement reddens it
and the four bridges do not go near it.

Promotion check on the four: **exactly 4 registrations removed, 4 triples gone,
0 rows flip `owns_slot: false → true`.** No duplicate exists on any of them.

**Retired in source.** Do not extend it to the classes.

### 4.1 Verification of the source deletion

| arm | result |
|---|---|
| `CRATONVM_ARGS=--jdk-only` | 105/107 — `RTreeRangeGc`, `RJdkJmx` (identical to control) |
| `SUITE=all` | 106/107 — `RTreeRangeGc` |
| `SUITE=core` | 66/67 — `RTreeRangeGc` |

No new failures at any arm; both names are the pre-existing pair from
`WORKER-3-NOTE-6` §5.

**And a trap the promotion checker walked straight into.** Run against the
PRE-deletion control dump, it reported **957 promotions** — all false. Deleting
51 lines from `lib.rs` renumbers every later registration, and the checker keys
slot ownership on `registered_by`, which is `file:line`. The tell is in its own
output: `lib.rs:28069 -> lib.rs:28028`, same file, constant −41 delta.

A post-deletion dump cannot be promotion-checked against a pre-deletion dump at
all. The valid checks, both run:

* the GATE run on the unmodified binary (line numbers identical): **0
  promotions**;
* gate-run registry vs source-deleted registry, line numbers dropped: **10075
  triples on both sides, 0 present in only one, 0 whose owning FILE differs** —
  the deletion is equivalent to the thing that was measured, and does nothing
  else.

## 5. Where the three blocks now stand

| block | rows | state |
|---|---:|---|
| `java/lang` boxes | 4 | **CLOSED** — retired, real bytecode serves all four |
| `java/lang` exceptions | 62 / 651 | **REFUSED**, three granularities, evidence above |
| `java/lang/invoke` | 56 | REFUSED — capability gap (`NOTE-4` §3) |
| `StringBuilder`/`StringBuffer` | 57 | REFUSED — `NOTE-4` §4, one defect fixed in `NOTE-6` |

## Index rows for `INDEX.md` (H0 to place)

* `WORKER-3-NOTE-7` §2 — deleting the `lib.rs` exception extras loop would
  promote **60** `owns_slot: false` bodies (`lang_misc:3120`/`:3156`, 9 of 39
  owning each). Work-order step 3 FAILS; H22's trap at 60 rather than 16
* `WORKER-3-NOTE-7` §3 — retiring 62 triples and retiring all 651 registrations
  cost the SAME two vectors; the price is entirely in `getMessage`/`toString`
* `WORKER-3-NOTE-7` §4 — the box block is CLOSED at 4 rows: all four are the
  only non-`Intrinsic` registrations on `Integer`/`Long`, and a class-scoped
  retirement reddens `RJdkReflBox` because the intrinsic `valueOf(I)` backs the
  identity cache
* `WORKER-3-NOTE-7` §1 — a TRIPLE gate is not a SITE gate: gating 62 triples
  removed 122 registrations. Pricing one call site needs `Location::caller()`
* `WORKER-3-NOTE-7` §4.1 — a promotion check across a SOURCE deletion is invalid
  if it keys on `file:line`: deleting 51 lines reported 957 false promotions.
  Compare the gate run against the deleted build with line numbers dropped
