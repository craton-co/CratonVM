# H22-1 — `java/util/HexFormat` is retired, and 16 of its 42 registrations could never have run

**Status: LANDED (source) — MEASURED, one 30-assertion differential probe in
four arms plus two `--dump-native-registry --explain-jdk-only` dumps and one
exact-invocation census.** All runs on the prebuilt
`C:/craton/cratonvm-r5.exe` (42,434,048 bytes, 2026-08-20 21:57), which does
**not** contain this lane's edit. Oracle: HotSpot 25.0.3+9 at
`C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot`. **This lane may not build,
so nothing here is verified against a binary carrying the change** — see §7.

Lane H22, 2026-08-21. Acts on `H14-3` N3.

---

## 0. The gap this worktree was cut across

Cut at `26e4b5db4`. `git merge --ff-only claude/jdk-only-mode-handoff-09b48c`
fast-forwarded **107 commits** to `3a6cc90fd`. What was in the gap and matters
here:

* `ef70b2510`, `15df1c9e7`, `de00c0fad` — the whole `H14` trilogy: the 1402-row
  classification, the ranked candidate list, and the thirteen priced arms this
  lane was sent to act on. Without the merge this lane would have had no table.
* `dfd4718c7` (`H18-1/2/3`), `3851ad3a2` (`H14-1` independent check),
  `2a944b0c1` (`H0-4` second correction), `828ea254f` (INDEX rows).
* `a73aea08b` — the opcode/`getClass()`-alias fix.
* `9eef86699` — the round-5 baseline that moved the corpus denominator to 105.

## 1. What was there

`java/util/HexFormat` carried **42** native registrations from **two**
registrars. MEASURED, `--dump-native-registry --explain-jdk-only` under
`--jdk-only`:

| registrar | registrations | `owns_slot: true` | `owns_slot: false` |
|---|---:|---:|---:|
| `phases_late.rs::register_p64_hex_format` | 26 | **26** | 0 |
| `lib.rs::register_hex_format_real_jdk_natives` | 16 | 0 | **16** |

**Every registration in the second registrar had already lost its slot** — to
the first one, which that same function called *last*, on purpose, because
`register()` is last-write-wins (`W8-C15-2`). Its only live effect was that
tail call. And that tail call was `register_p64_hex_format`'s **only shipping
call site**; its other caller, `register_phase64_natives`, is allow-listed
synthetic-only in `registrar_reachability.rs`.

That is the shape `H14-1` §5 warned about, met in the field:

> only the `owns_slot: true` registration is reachable. A retirement aimed at a
> losing registration changes nothing and measures as "no effect".

Here it is the **converse and it is worse**: retiring only the winner would have
*promoted* sixteen dead bodies — bodies whose own tombstone-worthy defect list
(`W8-C15-2`) is that they never read the receiver, so every `with*` setting was
inert, and two of them panicked on ordinary bytecode input. A row-count-driven
retirement of the 24-row registrar would have replaced a correct implementation
with a broken one and measured as a 24-row win.

## 2. What was measured before deleting anything

### 2a. Positive control — the bodies were live

`H14-3` §6 and this directory's standing trap both say a zero proves nothing
alone. MEASURED with `--nojit CRATONVM_DISABLE_INTRINSICS=1`, the configuration
`G33-1` requires for an exact `invocations` column, running the probe of §2b:

```
group                               regs inv>0 sum(inv)
HexFormat                             42    21       39
```

**21 of the 42 registrations were invoked, 39 dispatches.** The retirement is
not a no-op on paper.

### 2b. The probe, and the arm that prices the retirement

`CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HexFormat` makes contract §1.4
**enforced** for that class, which is exactly what deleting the registrations
does permanently. The probe is 30 assertions over the whole public surface —
`formatHex(byte[])`, ranged `formatHex`, `parseHex`, `withUpperCase` /
`withLowerCase` / `isUpperCase`, `ofDelimiter` / `delimiter`, `withPrefix` /
`withSuffix` / `prefix` / `suffix`, all five `toHexDigits` overloads plus the
counted one, `toHighHexDigit` / `toLowHexDigit`, `isHexDigit` (including the
non-ASCII `U+0661` case `W8-C15-2` recorded as answering `true` through an
`as u8` truncation), `fromHexDigit` / `fromHexDigits` / `fromHexDigitsToLong`,
`toString`, `equals`, `hashCode`, and both panic paths — diffed against HotSpot
**on stdout only**, because `2>&1` puts VM tracing in the diff.

| arm | probe lines | differing lines vs HotSpot |
|---|---:|---:|
| unarmed | 517 | 16 (three pre-existing defects, §5) |
| `java/util/HexFormat` armed | 517 | **16 — byte-identical to unarmed** |

**Zero HexFormat assertions move.** Combined with `H14-3` §1 row 5, which
priced the same prefix over the 104-vector corpus at a **net cost of zero
vectors**, the retirement is measured free by two independent instruments with
different failure modes.

## 3. What landed

Commit `969f0de8f`, `native-builtins/src/lib.rs` (−302 / +41):

* **`fn register_hex_format_real_jdk_natives` deleted** (293 lines), along with
  its unconditional call from `register_essential_natives`. It also fabricated
  its receiver with `try_alloc_concurrent_synthetic("java/util/HexFormat", 4)`,
  so an untyped-allocation site goes with it — relevant to the ratchet
  `50c13bf24` added.
* **`register_p64_hex_format` is KEPT and is now synthetic-only.** Under the
  `synthetic-jdk` feature there is no `java/util/HexFormat` bytecode to fall
  back to, and it stays reachable from `register_phase64_natives`. **Retiring a
  native is not the same as deleting it**, and this is the distinction
  `narrowing-a-natives-registration-to-a-flag-drops-the-mode-with-no-fallback`
  exists to protect: the shipping build drops it because bytecode exists, the
  synthetic build keeps it because bytecode does not.

One of the 26 retired rows is `parseHex(Ljava/lang/String;)[B`. MEASURED with
`javap -p java.util.HexFormat`: JDK 25 declares `parseHex(CharSequence)`,
`parseHex(CharSequence,int,int)` and `parseHex(char[],int,int)` and **no
`String` overload at all**. `javac` emits the `CharSequence` descriptor for a
`String` argument, so no bytecode could ever have named that registration. It
is one of `H14-1` §4's 156 "registered on a class that does not declare the
method" rows, and the answer to that record's *"find out what they were for"* is,
for this one, **nothing**.

## 4. The gate this moved, and why the old control would have passed vacuously

`native-builtins/tests/registrar_reachability.rs` pins a two-sided control:
`register_pe_panama` MUST be synthetic-only, and `CONTROL_NEGATIVE` MUST NOT be.
`CONTROL_NEGATIVE` was `register_p64_hex_format` — the exact registrar this
change moves into the synthetic arm.

Two edits, both prescribed by the test's own failure messages:

1. `register_p64_hex_format` **added to `SYNTHETIC_ONLY_CLOSURE`**. That list's
   doc comment calls itself "a RATCHET IN BOTH DIRECTIONS" and asks for the
   entering name to be recorded; this is a name entering.
2. `CONTROL_NEGATIVE` **re-pointed** to `register_throwable_subclass_natives`,
   whose shipping reachability is structural (called from
   `register_essential_natives`, which the real-JDK entry point
   `register_essential_natives_with_shims` calls unconditionally) and which
   `H22-2` establishes is not about to move.

And one edit nobody asked for, which is the point of this section. The
assertion was `!a.synthetic_only.contains(CONTROL_NEGATIVE)`. **A deleted
registrar is also "not synthetic-only".** Had this lane deleted
`register_p64_hex_format` outright instead of moving it, the negative control
would have stayed GREEN while measuring nothing at all, and the doc comment's
own claim — "so this file cannot pass by finding nothing" — would have become
false without a single test failing. The control now additionally asserts
`a.shipping.contains(CONTROL_NEGATIVE)`, so a vacuous control fails loudly:

```rust
assert!(
    a.shipping.contains(CONTROL_NEGATIVE),
    "NEGATIVE CONTROL VACUOUS: ... the assertion below would pass without \
     measuring anything."
);
```

This is a general shape and it is worth carrying: **a control that pins an
absence is satisfied by deletion.** Any `!contains` control needs a membership
assertion beside it.

## 5. Three defects the probe found on the way, none of them HexFormat's

The unarmed 16-line diff is not noise. MEASURED, `--jdk-only`, no dial:

| assertion | HotSpot | CratonVM |
|---|---|---|
| `new ParseException("pe",3).getStackTrace()[0].getMethodName()` | `throwables` | **`<init>`** |
| `new TypeNotPresentException("T",null).getMessage()` | `Type T not present` | **`T`** |
| `new Exception().setStackTrace(null)` | `NullPointerException` | **no throw** |

The second is a native `<init>` writing the raw type name into `detailMessage`
where the JDK constructor builds a sentence. **Arming the throwable family
FIXES it** (§`H22-2` §3), which is the positive control that the dial bites at
all. The third is wrong armed *and* unarmed, so it is not a shadow — it is a
missing null check on a path both modes take. Neither is diagnosed here.

### 5a. One side effect worth telling the GC investigation about

The deleted `of()` fabricated its receiver with
`try_alloc_concurrent_synthetic("java/util/HexFormat", 4)`. Grepping the tree
for the class name afterwards turns up exactly one non-registration mention,
`vm/src/runtime/interpreter.rs:573`:

> the pre-GC young `0x4` was measured on `java/util/HexFormat` **fld[1]** and
> `java/util/logging/Level`

That is a stray-write victim watchlist, and `fld[1]` is slot 1 of a 4-slot
object — i.e. the `prefix` slot of the fabricated carrier this commit removes.
`zero-header-object-defeats-the-sweep-zero-span-screen` is the shape to compare
it against. **Nothing is claimed here**: the watchlist entry is a debug aid, not
a dispatch path, and it is one of two named victims. But whoever owns that
investigation should know that one of the two objects it watches no longer
exists in the shipping build, and that a `--jdk-only` run after this commit is a
free negative control for them.

Grep also confirms the retirement is complete: **no interpreter intrinsic, JIT
thin helper or `native_override` entry binds `java/util/HexFormat`
independently of the registry** (`jit-thin-direct-helpers-reimplement-natives`
is the failure mode being ruled out). The registry was the only binding, so
deleting the registrations is the whole retirement.

## 6. Census prediction

`native-shadows-bytecode` counts rows a workload observed, not registrations.
Of the 42, `H14-1` attributed **24** to `register_p64_hex_format` in the
104-vector corpus and **0** to the loser (its bodies never ran). So:

* **`native-shadows-bytecode` should fall by 24**, from 1402 to **1378**, with
  no other change.
* `synthetic-native-registered` should be unaffected (these were `Intrinsic`
  and `Bridge`, not synthetic-stub).
* The registration total drops by 42, from 10,418 to **10,376** under
  `--jdk-only`.

**ARGUED, not measured** — this lane cannot build. It is the narrowest
confirmation available and the orchestrator should check it against the number,
not against a range.

## 7. What this does NOT establish

* **Nothing here was compiled.** `cargo check`, `cargo test -p native-builtins
  --test registrar_reachability`, and the synthetic-jdk vm gate have all NOT
  been run against this commit. The edits are mechanical (one whole `fn`
  deleted at verified boundaries, one call site, two constants and one new
  assertion in a test), the crate allows `dead_code` and `unused_imports`
  workspace-wide, and brace balance was checked before and after — but that is
  an argument, not a green build.
* **The probe is 30 assertions, not the JDK's HexFormat test suite.** It covers
  every public method and both documented panic paths; it does not cover
  `formatHex(Appendable,...)`, `parseHex(char[],int,int)`, or any locale or
  concurrency dimension.
* **The corpus has no HexFormat-heavy vector.** `H14-3`'s zero and this
  probe's zero are two screens, not a proof.
* **Compatible mode is untouched by the dial**, so the §2b arm says nothing
  about it. The *source* change does affect it — the registration is gone in
  both modes of the real-JDK build. MEASURED mitigation: a Compatible-mode
  `--dump-native-registry` reports `image_has_class: true` for all 42
  `java/util/HexFormat` registrations, so real bytecode is present in that mode
  too. That is the premise, checked; it is not a run.

## 8. NOMINATIONS

* **N1 — build and run the two gates this lane could not.** `cargo test -p
  native-builtins --test registrar_reachability` (both new assertions) and the
  synthetic-jdk vm gate (which is the arm that still uses
  `register_p64_hex_format`). Then re-run the census and check §6's `1378`.
* **N2 — audit every `!contains` control in the tree for the vacuity in §4.**
  `registrar_reachability.rs` had one; it is unlikely to be the only one. The
  pattern to grep for is an `assert!(!` over a set membership with no
  corresponding positive assertion on the same name.
* **N3 — re-price the remaining `H14-3` zero-cost rows against a source
  retirement, not the dial.** `H22-3` gives four measured reasons the two are
  not the same question, and `H22-2` gives two cases where the answer differs.
* **N4 — `TypeNotPresentException.<init>` writes the wrong `detailMessage`**
  (§5). It is a one-line native defect, invisible to all 105 vectors, and the
  throwable arm already shows what the right answer looks like.
* **N5 — `Throwable.setStackTrace(null)` does not throw** (§5), in both the
  native and the bytecode path. The bytecode-path half is a VM defect, not a
  shadow, and is the more interesting one.

## 9. `INDEX.md` row

`docs/known-issues/jdk-only/INDEX.md` is outside this lane's paths.

```markdown
- [H22-1](H22-1-hexformat-retired-and-sixteen-of-its-registrations-could-never-run-20260821.md) — `LANDED` · **MEASURED, 4 probe arms + 2 registry dumps + 1 exact-invocation census.** `java/util/HexFormat` is retired to real JDK bytecode — the first of `H14-3`'s five free retirements to actually land. It needed TWO registrars deleted, not one: of its 42 registrations, `register_p64_hex_format` owned all 26 reachable slots and `lib.rs::register_hex_format_real_jdk_natives` held **16 that were `owns_slot: false` — every single one** — so retiring the winner alone would have PROMOTED sixteen bodies `W8-C15-2` had already condemned. One retired row, `parseHex(Ljava/lang/String;)[B`, is a descriptor JDK 25 does not declare at all. Cost measured twice: `H14-3`'s corpus arm (net zero vectors) and a new 30-assertion HexFormat probe armed with `CRATONVM_ENFORCE_NATIVE_SHADOW`, **byte-identical to HotSpot 25.0.3+9**; the positive control is 39 native dispatches in the same run with `--nojit CRATONVM_DISABLE_INTRINSICS=1`. §4 is the general finding: `registrar_reachability.rs`'s negative control asserted `!synthetic_only.contains(name)`, and **a deleted registrar is also "not synthetic-only"** — the control would have passed vacuously, so it now asserts shipping membership too. Predicts `native-shadows-bytecode` 1402 → **1378**. NOT BUILT.
```
