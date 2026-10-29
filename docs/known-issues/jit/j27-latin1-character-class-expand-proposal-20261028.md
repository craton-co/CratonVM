> **j27 wave 8 (lane W8CHA, 2026-10-28): REVIEWED, PENDING HOST VERIFICATION.** No code change. Re-read `jit/src/ir/latin1_char_class.rs`: the bodies match JDK 25 (`isWhitespace` = `(A & 0x7000) == 0x4000`, `isUpperCase` / `isLowerCase` = `B & 2` / `B & 1`, i.e. `Other_Uppercase` / `Other_Lowercase` included: `ª` U+00AA and `º` U+00BA are lower case; `isLetter` / `isLetterOrDigit` = the type mask over `A & 0x1F`); the guard `cp >>> 8 == 0` is `CharacterData.of`'s, so negative and non-Latin-1 code points always call; the proof `x & c` with `c` in `[0, 255]` is sound for an `Int` argument (checked: `arg_is_int`); the miss call runs from the pre-expansion memory state and plants its handler entry like `Reference.get`'s. JDK 21 boots read the same Latin-1 answers (no Latin-1 property changed between Unicode 15 and 16). Stays default OFF until the wave-4 measurement step (4) below is run.

> **j27 wave 4 (lane LATIN1, 2026-10-28): FIXED IN CODE behind a default-OFF switch, PENDING HOST VERIFICATION.**
> New module `jit/src/ir/latin1_char_class.rs` (tests in `latin1_char_class_tests.rs`), wired by one
> arm in `IrBuilder::build`'s `0xb6 | 0xb8` invoke arm (after `try_reference_get_intrinsic`, ahead of
> the splice). `Character.isDigit` / `isLetter` / `isLetterOrDigit` / `isWhitespace` / `isUpperCase` /
> `isLowerCase`, both `(C)Z` and `(I)Z`, become `If(cp >>> 8 == 0)` (= `CharacterData.of`'s own test):
> the true edge is a branch-free 256-bit bitmap read built from the JDK 25 `CharacterDataLatin1.A[]` /
> `B[]` tables (copied by script from Temurin 25.0.3 `src.zip`, evaluated with the JDK bodies, every one
> of the 256 answers per predicate checked against independent references in the unit tests); the false
> edge is the site's own `Op::Call` (the `Reference.get` diamond: `Merge` + value / memory `Phi`). A code
> point proven Latin-1 at build time (a constant in `[0, 255]`, or `x & c` with such a `c`) gets the
> table answer with no branch and no call. No planner change: the site keeps its `invoke_info` row.
> Switch `CRATONVM_JIT_IR_LATIN1_CHAR_CLASS_EXPAND` (default OFF, `=1` on; also under
> `CRATONVM_JIT_IR_SCALAR_INTRINSICS`). Opt-in because it is unmeasured: the "intrinsic loses to bytecode"
> rule says measure first. Probe `tools/probes/jit/j27/J27Latin1CharRows.java`.
> **Verify:** (1) `cargo test -p cratonvm-jit --lib ir::latin1_char_class` (9 tests, one x86_64-only);
> (2) `java -cp <dir> J27Latin1CharRows > hs.txt`, then `cratonvm --cp <dir> J27Latin1CharRows | diff - hs.txt`
> under {default, `CRATONVM_JIT_IR_LATIN1_CHAR_CLASS_EXPAND=1`} x {`--jdk-only`, `--compatible`}: no diff;
> (3) engagement: with the switch on and `CRATONVM_DBG_JITC=1`, look for the `[ir] Character.isDigit
> Latin-1 table inline` lines (`call on the non-Latin-1 arm` for `latin1Digit`, `proven Latin-1, no call`
> for `maskedDigit`); (4) measure `R24RegexSplitRows` `split-ws` / `regex-*` (instructions per op) with the
> switch OFF and ON before any default flip.
> Expected HotSpot 25 facts, computed from the JDK 25 tables (`latin1` and `masked` lines are identical):
> `fact latin1 isDigit n=10 h=1584875013 bad=0`, `isLetter n=117 h=1011346677`,
> `isLetterOrDigit n=127 h=1802733584`, `isWhitespace n=10 h=1786788403`, `isUpperCase n=56 h=-324307811`,
> `isLowerCase n=61 h=1404983954` (each `bad=0`); `fact regex-split-ws n=7 h=-713410381 bad=0`,
> `fact regex-digit-runs 3`, `fact regex-upper 7`, `fact regex-lower 18`, `fact regex-letter-or-digit 29`,
> `fact stream-is-letter chars=25 codepoints=25`. The `fact char <p> ...` (0..0xFFFF) and `fact all <p> ...`
> (-16..0x11000F) lines are (host): take them from HotSpot, each with `bad=0`.

# Proposal: expand the Latin-1 `Character.is*` classification calls in the optimizing tier

| | |
|---|---|
| **Status** | IMPLEMENTED, default OFF (j27 wave 4, lane LATIN1; filed j27 wave 2, lane INTRINS) |
| **Area** | JIT IR tier (`jit/src/ir/pure_expand.rs` would host it), `native-builtins/src/lang_math.rs` `register_character_classification_natives` |
| **Source** | `j27-intrinsic-coverage-vs-hotspot-20261028.md` (the one missing row that touches the regex / split pages) |

## Evidence

HotSpot 25's C2 intrinsifies `CharacterDataLatin1.isDigit` / `isLowerCase` / `isUpperCase` / `isWhitespace`
(`vmIntrinsics` `_isDigit` ...), which `Character.isDigit(int)` etc. reach for a code point below 256. Here
`Character.isDigit(C)Z` / `(I)Z`, `isLetter`, `isWhitespace`, `isUpperCase`, `isLowerCase` are registered
`Intrinsic` natives, so every compiled call is a native-funnel crossing; regex `CharPredicate`s (`\s`, `\d`,
`Character::isWhitespace`-style lambdas, `split("\\s+")`) make one per input character
(`r23-w2-regex-rows-gap-is-per-call-cost-20261021.md`, `r23-w5-compiled-jdk-string-split-and-regex-bodies-7-13x-20261021.md`).

## What would fix it

A `pure_expand` family per predicate: for `ch < 256` the JDK 25 Latin-1 answer (`CharacterDataLatin1`'s `A[]`
property table is a `static final int[]` -- read it as a constant-folded array load, or hard-code the
`isWhitespace` / `isDigit` sets, which are tiny: `isDigit` is `'0'..'9'` only below 256; `isWhitespace` is
`\t \n \u000B \f \r \u001C..\u001F space`), and a call (the existing native) on the `ch >= 256` arm. That
needs a branch with a call on one arm, which the current branchless expansions do not build: the builder
would plant an `If` / `Merge` / `Phi` around the call row the planner then must keep. Measure on
`R24RegexSplitRows` `split-ws` and `regex-*` (instructions per op) before flipping; register no-JIT-only
first if the bytecode body wins (memory: "intrinsic loses to bytecode").
