# CratonVM vs HotSpot Divergence Log — CLOSED (2026-07-28)

> **Status: ✅ RESOLVED — retired.** Every divergence this log recorded
> (DIV-001, DIV-002, DIV-003) is fixed and pinned by a differential fixture that
> runs `main` under both CratonVM and HotSpot JDK 25 and requires byte-identical
> stdout. The log is retired here rather than kept under `docs/known-gaps/`
> because it has no open entries left; the *live* record of divergences is now
> the harness itself — `vm/tests/differential.rs` plus the report it writes to
> `bench/differential-divergences.json`.
>
> Where to add the next one: write a fixture class under
> `vm/tests/resources/cratonvm/`, add an `#[ignore]`d
> `assert_main_matches_hotspot("cratonvm/YourFixture")` test, or point the
> `DIFFERENTIAL_CLASSES` env var at it for an ad-hoc sweep.

This document tracked known behavioral divergences between CratonVM and HotSpot
(OpenJDK 25) discovered through differential testing (`vm/tests/differential.rs`).

Each entry records the class, method, expected (HotSpot) behavior, actual
(CratonVM) behavior, root cause, and resolution status.

---

## Active Divergences

None. All three entries moved to [Resolved](#resolved-divergences) on
2026-07-28.

---

## Resolved Divergences

### DIV-001: String case mapping ignored the `Locale` — RESOLVED 2026-07-28

| Field     | Value |
|-----------|-------|
| Class     | `java/lang/String` |
| Method    | `toUpperCase(Locale)` / `toLowerCase(Locale)` (and the no-arg forms) |
| HotSpot   | Applies `SpecialCasing.txt`'s locale-conditional rules |
| CratonVM   | Ignored the `Locale` argument entirely — root-locale mapping only |
| Fix       | `native-builtins/src/case_map.rs` + `lang_string::string_case_impl` |

**What was actually wrong.** The original entry blamed "ASCII-only uppercasing
in the synthetic String stub", which was already stale: the natives used Rust's
`str::to_uppercase()` / `to_lowercase()`, i.e. Unicode's *unconditional* full
mappings, and those agree with HotSpot byte-for-byte (`straße → STRASSE`,
`ΣΣ → σς` with the final sigma, `İ → i̇`). The real gap was narrower and
sharper: every overload dropped its `Locale` argument on the floor, so the
**locale-conditional** half of `SpecialCasing.txt` — Turkish/Azeri dotted and
dotless I, and the Lithuanian retained dot above — never ran.
`"TITLE".toLowerCase(Locale.forLanguageTag("tr"))` returned `"title"` where
HotSpot returns `"tıtle"`.

**Fix.** `case_map.rs` is a literal port of `java.lang.ConditionalSpecialCasing`:
the same 17-row entry table, the same five context conditions (`Final_Cased`,
`After_Soft_Dotted`, `More_Above`, `After_I`, `Not_Before_Dot`), and the same
"a language-specific row wins over the language-independent one" lookup. Only
`tr`/`az`/`lt` take that path; every other locale keeps the already-correct Rust
mapping, which bounds the new code's blast radius. All seven registration sites
(the real-JDK essential set, the synthetic overrides, the `phases_early`
`Locale` overloads, and the `StringLatin1` delegate) now share one
implementation, `lang_string::string_case_impl`. That includes
`StringLatin1.toLowerCase(String,byte[],Locale)` — the JIT-reachable compact
path, which had been forwarding only `args[..1]` and so was root-locale-only.

Two caching details matter. The locale-dependent path bypasses the per-receiver
`ascii_case_string` memo, which is keyed by receiver + direction: without that,
`s.toLowerCase(TURKISH)` would be served the answer cached for an earlier
`s.toLowerCase()`. And the sites did not previously *agree* on memoising — the
synthetic overrides did, the real-JDK essential set and the `phases_early`
overloads allocated fresh each call — so the shared body takes a `memoize` flag
and each site keeps what it had, leaving locale handling as the only behaviour
this changed. The default locale's language is resolved once rather than per
call, because `String.toLowerCase()` is far too hot to carry a system-property
lookup.

The no-arg overloads are `toXxxCase(Locale.getDefault())` in the JDK; they
resolve the default language from the `user.language` system property rather
than *calling* `getDefault()` (which can allocate and run `<clinit>` from inside
a String native), so `-Duser.language=tr` behaves as it does on HotSpot.

**JIT residual closed 2026-07-28.** Fixing the natives was not enough: the JIT
emits a *thin direct call* for `String.toLowerCase(Locale)` and
`StringLatin1.toLowerCase(String,byte[],Locale)` in place of the native-dispatch
round trip, and those two helpers (`jit_string_locale_to_lower_direct` /
`jit_string_latin1_to_lower_direct`) carried their own private copy of the fold
— one whose doc comment said outright that "Locale is currently unused". So the
same call returned `tıtle` interpreted and `title` once its caller tiered up: a
method's answer changed as it got hotter. `toUpperCase` has no such helper and
was never affected, which is why the divergence looked half-present.

Both helpers now delegate to the same `lang_string` implementation as the
interpreted native. **The lesson is the general one:** a native whose semantics
change must be checked for a JIT fast path that reimplements it — a
correctness-critical argument that a thin helper drops is invisible to any test
that does not warm the caller up.

**Verification.** `cratonvm/DiffLocaleCase` — 20 inputs × 6 locales × both
directions, plus `forLanguageTag` construction, the cache-poisoning case, and a
5000-iteration warm-up that tiers the call sites up — byte-identical to JDK 25.
(The JIT divergence reproduces from ~3000 iterations; a fixture that called each
case once could not have seen it.) Unit tests in `case_map.rs` carry the same
HotSpot-captured expectations without needing a JDK.

---

### DIV-002: `Double.toString` / `Float.toString` layout — RESOLVED

| Field     | Value |
|-----------|-------|
| Class     | Various |
| Method    | `System.out.println(double)`, `String.valueOf`, `Double.toString`, … |
| HotSpot   | JLS layout: plain decimal in `10^-3 .. 10^7`, else `d.dddEexp` |
| CratonVM   | Was Rust `format!("{}")`, which never switches to scientific form |
| Fix       | `types/src/float_format.rs` (VM side) + `format_value` (harness side) |

**Root cause.** Rust's `Display` for `f64`/`f32` picks the same shortest
round-tripping digits Java does, but never switches to scientific notation, so
`1e7` printed as `10000000` instead of `1.0E7`.

**Fix.** `cratonvm_types::java_double_to_string` / `java_float_to_string`
implement the JLS layout rule over Rust's Ryū digit generator, with a
fixed-precision retry loop for subnormals (where Ryū's shortest form,
`5e-324`, is shorter than the JDK's `4.9E-324`). Every printing path — the
`println` natives, `String.valueOf`, the boxed `toString`, `StringBuilder.append`
and string concatenation — routes through it.

**Residual closed 2026-07-28.** The *harness* still formatted CratonVM's
`Double`/`Float` return values with Rust's `Display`, so `differential_run`
reported a divergence for every value outside the plain-decimal window even when
the VM had produced the correct string. `format_value` now uses the same
`java_*_to_string` functions the VM does — see
[the harness residuals](#harness-residuals-closed-2026-07-28), which turned out
to account for every remaining "divergence" in the suite.

**Verification.** `cratonvm/DiffFloatFormat` — 22 doubles × 13 floats through
six printing paths each, including the specials, both subnormal extremes and
both signed zeros — byte-identical to JDK 25, in the interpreter and after JIT
warm-up.

---

### DIV-003: JEP 358 helpful NullPointerException messages — RESOLVED

| Field     | Value |
|-----------|-------|
| Class     | Various |
| Method    | Every null-dereferencing opcode |
| HotSpot   | `Cannot <action> because "<expr>" is null` (JEP 358) |
| CratonVM   | Generic messages, then action-only inside merge blocks |
| Fix       | `vm/src/runtime/exceptions.rs::helpful_npe` (see the feature design) |

**History.** Increments 1–5 (`docs/feature-designs/jep358-helpful-npe.md`) built
the action half for every opcode, the bounded expression reconstruction, real
`LocalVariableTable` names, the `-XX:±ShowCodeDetailsInExceptionMessages` flag,
and flipped the default on after a 41-case differential pass.

**Residual closed 2026-07-28.** The operand-stack reconstruction simulated
straight-line from the trapping instruction's *basic-block leader*, starting
from an empty stack. That is correct only when the leader begins with an empty
operand stack — which a control-flow **merge** point does not: the join of a
ternary, of a `&&`/`||` short-circuit, or of a `switch` arm leaves the
predecessors' value on the stack. The very first pop then underflowed and the
whole reconstruction bailed to the (valid, but less detailed) action-only
message. Since `T x = cond ? a : b; x.deref()` is one of the most common shapes
in real code, this cost the `because "<expr>" is null` clause on a large slice
of real NPEs — with or without the JIT; the earlier "JIT-only" reading of it was
wrong.

An underflow is now treated as *information* — "this operand predates the
block" — rather than as a failure: the surviving entries stay correctly aligned
relative to the top of the stack, which is how every caller indexes them, and an
operand that really does reach below the block boundary still reports an unknown
producer and still yields HotSpot's action-only message. Arithmetic, conversion
and comparison opcodes are now modelled too (they routinely sit in a trapping
statement's own prefix, e.g. `a[i + 1]`); the category-ambiguous stack shufflers
(`dup2`, `dup_x1`, `pop2`, `swap`) are deliberately left unmodelled, because a
mis-shaped stack would name the *wrong* value — worse than naming none.

**Second residual closed 2026-07-28 — the opt-out flag.** With
`-XX:-ShowCodeDetailsInExceptionMessages`, HotSpot's `getMessage()` is `null`
for an implicit-dereference NPE. CratonVM instead fell back to its pre-JEP-358
diagnostic text (`arraylength null (in Foo.main([Ljava/lang/String;)V pc=7)`),
and the *invoke* site — whose message shipped in increment 1 as
"unconditionally on" — kept the full JEP-358 string. So the flag that exists to
match HotSpot diverged from it.

The fix keeps the legacy strings exactly where they were wanted and nowhere
else, by distinguishing three states instead of two. `helpful_npe_opcodes()` now
means "handle this the HotSpot way" and is true when the flag is on **or**
explicitly off; only the `-1` sentinel — an embedder, or the in-process Rust
test harness, that never wires the flag — is false and keeps the legacy
diagnostics. The two message builders return the empty string when
`helpful_npe_suppressed()`, which `throw_runtime_error` maps back to a `None`
message (the throw sites hand a `String` to `pop_object_ref_ctx_with` and so
cannot pass `None` themselves; no Rust-side path produces a deliberately empty
NPE message). A message that user code supplied — `Objects.requireNonNull(x,
"charset")` and friends — is untouched, as on HotSpot.

**Verification.** `cratonvm/DiffNpeMessage` — 40 cases covering every
null-deref opcode plus 13 merge-block shapes (ternary, `||`, `for`, `while`,
`switch`, post-`try/catch`, computed index) — byte-identical to JDK 25, in the
interpreter and after JIT warm-up; and `getMessage()` is `null` under
`-XX:-ShowCodeDetailsInExceptionMessages` on both VMs.

---

## Harness residuals closed 2026-07-28

Four of the five things the suite still reported as divergences were bugs in
the *harness*, not the VM. Worth recording, because each one produced a
confident, specific, and wrong accusation against the VM:

1. **The two sides ran different class libraries.** `run_cratonvm` built its
   config with `VmConfig::new()`, which is the *embedded* default — the
   synthetic JDK — while the HotSpot side ran a real `java`. Every synthetic
   stdlib gap was therefore charged to CratonVM as a behavioural divergence: the
   locale fixture "diverged" because synthetic mode cannot construct a
   `new Locale("en")` at all, and the NPE fixture "diverged" because its
   `Throwable.getMessage()` returns null. Both are true statements about the
   synthetic library and say nothing about the divergences under test. The
   harness now boots `VmConfig::for_launcher()` — the same real-JDK mode the
   shipping `cratonvm` launcher uses, from the same `$JAVA_HOME` the subprocess
   runs.
2. **Return values were rendered differently on the two sides.** `format_value`
   printed every reference return as the literal `"object"`, and `char`/`boolean`
   returns as their numeric operand-stack value — so `DiffString.concat`
   compared `object` against `hello world`, `charAt` `100` against `d`, and
   `equals` `1` against `true`. Nine of the fifteen comparisons in
   `diff_string_operations` could not have matched no matter what the VM did,
   which is why that test only *warned*. It now renders a reference return by
   reading the String, and takes `char`/`boolean` from the descriptor — and
   asserts.
3. **HotSpot's stdout was discarded** for value-returning methods while
   CratonVM's `printed_lines` were kept, so any method that both printed and
   returned was guaranteed to mismatch. The wrapper's output is now split at the
   last line (the return value) instead of thrown away.
4. **Concurrent tests raced on one wrapper file.** Every `run_hotspot` call for
   a value-returning method wrote, compiled, ran and then deleted
   `DiffWrapper__.java`/`.class` at one fixed path, so two tests running at once
   — the default `cargo test` layout — could delete each other's class between
   the compile and the run. The victim saw an empty HotSpot stdout and reported
   a divergence. Each run now gets its own directory. This one hid behind the
   *previous* bug: it could not surface while `diff_string_operations` merely
   warned, and it appeared the moment that test started asserting.

The fifth was the `Double.toString` formatting recorded under DIV-002 above.

The general lesson: a differential harness has two VMs *and itself* to get
right, and a mismatch is evidence about the pair, not about either side. Check
that both sides are configured alike, and that the comparison renders both sides
alike, before writing down a divergence.

The converse also bit, and is worth the same emphasis: a fixture that exercises
each case once tests only the interpreter. Two of the three entries here had a
JIT-path half that a single-shot fixture could not see — DIV-001's direct-call
helper, and DIV-003's action-only messages inside a branchy method. Warm the
call sites up.

---

## Testing methodology

The differential harness (`vm/tests/differential.rs`) works as follows:

1. For each test method, invoke it under CratonVM via `Vm::invoke()` and
   capture `printed_lines` + return value.
2. Invoke the same method under HotSpot via a `java` subprocess and capture
   stdout + printed return value.
3. Compare outcomes. Mismatches are recorded in `DivergenceReport` and
   serialized to `bench/differential-divergences.json`.

Run it with:

```
cargo test -p cratonvm-vm --test differential -- --ignored --nocapture
```

Tests are organized into:

- `diff_basic_arithmetic` — integer arithmetic (T4.10.2)
- `diff_string_operations` — String methods (T4.10.3)
- `diff_locale_case_mapping` — DIV-001 regression
- `diff_float_formatting` — DIV-002 regression
- `diff_npe_messages` — DIV-003 regression
- `diff_batch_from_env` — ad-hoc sweep over the classes named in the
  `DIFFERENTIAL_CLASSES` env var (comma/space separated, dotted or internal
  form)

Two harness invariants are easy to get wrong and were both wrong here at one
point: the CratonVM side must adopt the **launcher's** VM-flag defaults (an
in-process embedder that never calls
`env_cache::set_show_code_details_in_exception_messages` keeps the pre-JEP-358
strings, and then every NPE fixture "diverges" purely from configuration), and
the harness's own value formatting must match what `System.out.println` prints
on the HotSpot side.
