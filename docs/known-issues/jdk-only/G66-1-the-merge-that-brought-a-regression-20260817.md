# G66-1 — the merge that brought a regression, and a guard in a body someone had replaced

**Status:** MEASURED. **Provenance:** every row on both VMs. Oracle HotSpot
25.0.3+9-LTS. Binaries: `C:/craton/target-rel7` (this branch before the merge),
`target-rel8` (merged, no fix), `target-rel9` (merged + fix). Probe:
`scratchpad/g66/BdNaN.java`.

---

## 0. What happened

`dev` was merged into this branch by another agent, and then by me — twice,
because the agent had merged into an **older snapshot**: the remote branch was
missing this branch's last seven commits, so the two had diverged (the remote
had `dev`'s history, the local had the newer fixes). Both directions merged
without conflict, and two further `origin/dev` commits landed after.

Then the `--jdk-only` arm was re-run, and `RJdkIntrinsics3` went from **51 CK
lines to 6**, dying in `bigdec`.

## 1. The attribution, before any diagnosis

Same vector class file, two binaries:

```text
target-rel7  (this branch, pre-merge)   51 CK lines   = green
target-rel8  (merged)                    6 CK lines   = dies in bigdec
```

That A/B is the whole attribution and it cost one command. **A clean
`git merge` and a green build say nothing about behaviour** — this is the
"green build proves you broke nothing, not that you did something" rule with
the sign flipped: here the merge was clean, the build was green, and a vector
was broken.

## 2. The defect

```text
BigDecimal.valueOf(NaN)    HotSpot  NumberFormatException: Infinite or NaN
BigDecimal.valueOf(+Inf)   HotSpot  NumberFormatException: Infinite or NaN
BigDecimal.valueOf(-Inf)   HotSpot  NumberFormatException: Infinite or NaN
CratonVM (merged)          all three returned 0
```

`new BigDecimal(double)` is correct on both VMs — only the `valueOf` native
lost the guard. Three silently wrong numbers where the oracle refuses.

## 3. Why it happened, which is the part worth carrying

`0de751a13` fixed a real defect: `BigDecimal.valueOf(double)` was rendering
through Rust's `Display`, which is not `Double.toString` — it never uses
E-notation and prints `2.0` as `2` — so `valueOf(1e100)` produced a scale-0
integer with 101 digits. That fix is right and its replacement is right.

Its doc comment then said:

> Non-finite doubles cannot reach here: `valueOf` on them throws inside the
> `Double.toString`-fed `BigDecimal(String)` parse, and H2 screens them out
> before the call.

**That was true of the bytecode path and stopped being true the moment a
native took the `valueOf` slot.** There is no `BigDecimal(String)` parse left
to throw: `format_double(NaN)` is `"NaN"`, which flows through
`bd_parts_of_java_double_string` as a mantissa with no `.` and reaches
`BigInt::from_decimal`, which renders `0`.

**A guard that lives in a body you have replaced is not a guard you still
have.** The corrected note now says that where the old one made the claim,
because the next person to replace a body inherits exactly this trap.

This is the same species as `G64-1` (a duck test both shapes pass) and
`G59-1` (a synthetic slot map written into a real class): in all three the
wrong answer is produced silently at a site that looks correct in isolation.

## 4. The fix, and why three rows

One `is_finite` test, message transcribed — `Infinite or NaN`, no value
interpolated.

Three vector rows, not one. `RJdkIntrinsics3` **already had**
`bigdec:BigDecimal.valueOf(NaN)` — that is the row that caught this. But a fix
checking only NaN would satisfy it while both infinities kept answering a
number, so `+Inf`, `-Inf` and the exception MESSAGE are asserted as well
(`bigdec` 56 → 59).

## 5. All three arms on the merged tree

| arm | result | note |
|---|---|---|
| `--jdk-only` | **100 of 100** | denominator +1: `dev` added `RVarHandleAccess` |
| `SUITE=all` (Compatible) | 95 of 100 | same five as before the merge |
| `SUITE=core` (default) | 61 of 62 | same one, `RImmutableFactoryTypes` |

Failure sets identical **by name**, not merely equal in count.

## 6. Two corrections to this directory's own records

The G60-1 lane's `d7bb400bf` corrected `G62-1`, and both corrections are
inherited here rather than argued with:

* **"81 natives that win over real bytecode" was 58.** 23 of the 81 are rows
  recorded where the bridge LOST. My `G60-1` §1 quoted the report's own
  wording without checking what each row meant.
* **The 256-row observation sink SATURATED** on their application census —
  which my `G60-1` §4 flagged as a risk without knowing it had already been
  hit. A saturated sink reads as a complete one from the JSON.

## 7. NOMINATIONS

**N1 — the `--jdk-only` arm is not run against `dev`.** This regression
reached `dev` and sat there. It is caught by a vector that exists, in an arm
`dev` does not run — `G62-1` N2 asked for all three arms in CI and this is the
first measured instance of what that costs. One command would have held it at
the branch.

**N2 — `bd_parts_of_java_double_string`'s siblings.** The non-finite guard was
restored at one call site. Nothing was audited for the same shape elsewhere:
a native that took a slot from bytecode and inherited a doc comment describing
the bytecode's guarantees. The registry dump names every such slot, and the
comment pattern ("cannot reach here", "the caller screens", "throws upstream")
is greppable.
