# E27-1 — closing E18-1's three JIT-facing doors: the compile break, the fifth copy of the search rule, and one claim that was wrong

**Status: FIXED in `vm/src/jit/helpers.rs` and `jit/src/lib.rs` (lane E27,
2026-08-13), the only two files this lane owns and the only two it edited.
UNBUILT and UNRUN — this lane may not run `cargo` or the CratonVM binary, so
every CratonVM "after" below is marked PREDICTED. Every HotSpot column is
measured, in-session, with the transcript in `scratchpad/e27/E27Probe.java`.**

Oracle: Microsoft OpenJDK 25.0.3+9 (`java -version` confirmed in-session).

Input: `E18-1-the-jit-facing-string-doors-and-the-fourth-copy-of-one-search-rule.md`
§7 N1, N2, N3. **One of the three turned out to be a wrong diagnosis, not a
stale one** — see §3, and `[hypothes]`.

---

## 0. The tree should now compile as far as this lane's files are concerned

`vm/src/jit/helpers.rs` had the only call site of
`lang_string::jit_string_to_lower_case`, whose return type changed from
`Option<ObjectRef>` to `MethodCallResult`. That call site is adapted (§1).
Re-grepped at merge time as E18-1 N1 asked: `jit_string_to_lower_case` has
exactly **one** non-test caller in the tree, and it is the one this lane fixed.

```text
vm/src/jit/helpers.rs:11551          <- this lane's, now consuming MethodCallResult
native-builtins/src/lang_string.rs:4212   the definition
native-builtins/src/lang_string.rs:10592  a #[test] in the owning file
```

Both files were parse-checked with `rustfmt --edition 2021 --check` (not
`cargo` — `[cargo fm]`). Both parse. Neither edit introduces a NEW formatting
hunk: filtered to this lane's edited line ranges, `rustfmt --check` reports
nothing in `helpers.rs` 11500-11620 or `lib.rs` 7590-7660 / 8680-8790. The
repo is not rustfmt-clean overall (505 pre-existing hunks in `jit/src/lib.rs`,
31 in `helpers.rs`) and this lane did not touch that.

**What this lane believes will still NOT be green**, named explicitly:

* the separate `jit-api/` break another lane is closing — not this lane's, not
  inspected;
* **three tests in `jit/tests/intrinsic_string_search.rs`**, which pin the
  `indexOf(I)` intrinsic that §2 retires. They **compile**; they **fail at
  run time**. Exact text to fix them is N2a. Two of the three assert a
  measurably wrong answer today and had to change regardless of which fix was
  chosen — see §2.3, which is the `[freeze=lock]` shape.

Nothing else in this lane's two files is expected to move.

---

## 1. N1 — the compile fix, and the channel it had to use

`jit_string_latin1_to_lower_direct` now consumes `MethodCallResult` and routes
the `Err` arm through **`handle_jit_dispatch_error`** — the same signal path
`jit_hashmap_get_direct` uses at `:11473` and `:11491`, not a second mechanism.
That function stashes the throwable in the thread-local pending-exception slot
and returns `i64::MIN`, the deopt sentinel that makes the JIT caller's
post-invoke exception guard fire.

This is the whole reason the signature changed, and it is worth restating
because "just restore the old signature" is the tempting way to unbreak a
build: `NativeContext` has no throw channel, and `JIT_SIGNALS` /
`set_jit_pending_npe` are private to `helpers.rs`. An `Option<ObjectRef>`
return could only report the JDK's NullPointerException as `None`, which this
file turns into a **null String**. That is not a missing exception, it is a
wrong value — and it is a wrong value that appears only once the caller tiers
up.

Measured, OpenJDK 25.0.3+9, 200 000 warm iterations each:

```text
200k warm toLowerCase(null): npe/ok  =>  200000/0
200k warm toUpperCase(null): npe/ok  =>  200000/0
"AbC".toLowerCase()          =  abc          <- the no-arg overload must NOT throw
```

The oracle's answer does not depend on its tier. Ours did. **PREDICTED after:
it does not either.** The `toUpperCase` row is measured here because E18-1's
§1 only measured the lower twin; the contract is the same, and
`string_case_native` is the shared body, so the upper door is covered by the
same fix with no second edit.

Two smaller things the adaptation fixes, both inherited from the one-line
predecessor:

* a **non-zero `locale` address the heap does not recognise** used to fall
  through `is_object_address` into `None`, i.e. it was silently reclassified as
  "null Locale" and (before E18) as "default locale". It now bails to normal
  dispatch, the same treatment `source` gets six lines above. Bail, do not
  guess.
* every **`Err` was discarded**. An exception raised inside the case mapping
  became a null return and then an NPE at the *caller's* line, blaming the
  caller.

---

## 2. N2 — `StringIndexOfChar` was the fifth copy, and it is now retired rather than rewritten

### 2.1 The measured rule

`String.indexOf(int ch)` does not narrow `ch`. It gates on
`Character.isValidCodePoint` **first**, then scans for one code unit when
`ch <= 0xFFFF` and for the **surrogate pair** when `ch >= 0x10000`.

Measured, `scratchpad/e27/E27Probe.java`, OpenJDK 25.0.3+9. `MIXED` is
`"xзy𐐷z"` — Cyrillic з (U+0437, which is `0x10437 & 0xFFFF`)
at UTF-16 index 1, the real pair for U+10437 at 3-4, `'z'` at 5.

| call | HotSpot 25 | JIT intrinsic (before) | after (PREDICTED) |
|---|---|---|---|
| `"￿q".indexOf(-1)` | **-1** | `0` | -1 |
| `"￿q".indexOf(0xFFFF)` | 0 | 0 | unchanged |
| `"￿q".indexOf(0x1FFFF)` | **-1** | `0` | -1 |
| `"abc".indexOf(0x10061)` | **-1** | `0` (finds `'a'`) | -1 |
| `"abc".indexOf(0x110000)` | -1 | `0`… wraps | -1 |
| `"abc".indexOf(Integer.MIN_VALUE)` | -1 | `-1` (masks to 0) | unchanged |
| `MIXED.indexOf(0x10437)` | **3** | `1` (the masked half) | 3 |
| `MIXED.indexOf(0xD801)` | 3 | 3 | unchanged |
| `MIXED.indexOf(0xDC37)` | 4 | 4 | unchanged |
| `MIXED.indexOf(0x0437)` | 1 | 1 | unchanged |
| `MIXED.indexOf('z')` | **5** | 5 | unchanged |

Confirms E18-1 §2.1 independently, on a separately written probe.

**The row that rejects the plausible wrong fix** is `indexOf(-1)`. Also
measured, so the reasoning is not from memory:

```text
(char) -1 == 0xFFFF             =>  1     the cast really does produce 0xFFFF
Character.isValidCodePoint(-1)  =>  0     and the gate really is this
"￿q".indexOf(-1)           => -1     so the receiver holding U+FFFF does not matter
```

A fix written as `if ch <= 0xFFFF { scan(ch as u16) }` passes every other row
in the table and fails that one.

### 2.2 Why the intrinsic was dropped instead of re-implemented

The rule is now written **once**, in `native-builtins/src/lang_string.rs`'s
`code_point_needle` (`:5240`), feeding `index_of_units_from` /
`last_index_of_units_from`. This lane read it against the rows above and it
answers all of them.

`jit/src/lib.rs` **cannot call it**: the `jit` crate is below `native-builtins`
in the crate graph. So the only two moves available at this door were to write
the gate again — a fifth copy, which is the thing nine findings this session
say does not stick — or to stop handing the entry out and let ordinary
dispatch reach the one predicate. This lane took the second, which is E18-1
N2's own "cheapest and safest".

**Verified before landing, because a fix that moves a failure deeper can
worsen the count (`[nio=abstract]`): ordinary dispatch reaches a CORRECT
implementation in both modes.** Traced through the registry:

| mode | what answers `String.indexOf(I)I` | correct? |
|---|---|---|
| real-JDK (both `vm_init.rs` arms, `:2055` and `:2593`) | the **real JDK's own bytecode** — no native is registered at all | yes, by construction |
| synthetic-jdk (`vm_init.rs:1934`) | `native_string_index_of` (`lang_string.rs:1025`), the `code_point_needle` body | yes |

The real-JDK arm is worth spelling out because it is not obvious: a
`Bridge`-kind `java/lang/String` native is **dropped at registration** by
`native-api/src/registry.rs:6643-6649` (`drop_real_layout_synthetic`), so it
never enters the registry and `resolve_step1_native` returns `None`. Both
real-JDK arms set that flag before registering; they do not drift here
(`[2 rjdk arms]` checked, not assumed).

The cost is the inline scan on a hot method. Restoring it is a **codegen**
change, not a recognition change, and it does not need a copy of the rule
either: screen at run time on `ch < 0 || ch > 0xFFFF` and deopt, which admits
exactly the range where a single-code-unit scan already **is** the whole
answer. That is N2b, and it belongs in `x64/bytecode_walk.rs`.

### 2.3 A sixth copy, dead in both modes — and a test that froze the divergence

Two things fell out of the sweep that E18-1 did not have:

**`native-builtins/src/lib.rs:8174` and `:8195`** register `indexOf(I)I` and
`lastIndexOf(I)I` as closures built on `char::from_u32` and
**`s.chars().enumerate()`**. That is a sixth and seventh copy of the rule, and
they are wrong in a way none of the other five were: `chars()` yields a **code
point** index, not a UTF-16 index. `MIXED.indexOf('z')` would answer **4**
where HotSpot answers **5** — off by one for every supplementary character
before the hit. They also answer `-1` for every lone surrogate.

They answer in **no mode**: dropped at registration in real-JDK mode (same
`Bridge` rule as above), overwritten by last-write-wins in synthetic mode. So
this is dead code, not a live defect — but it is dead code that reads as a
working implementation, which is how the other four copies survived. N2c.

**`jit/tests/intrinsic_string_search.rs:570-590`** is the `[freeze=lock]`
shape, exactly:

```rust
        0x1_0000 + ('a' as i32), // supplementary; & 0xFFFF == 'a'
```

with `let needle = (ch & 0xFFFF) as u16;` as its own expected value. The test
derives its oracle from the sibling implementation rather than from the JDK,
and so asserts that `"a…".indexOf(0x10061)` finds `'a'` — which HotSpot says
is `-1`. **This test had to change whichever way N2 went**, so the three
failing tests are not a cost of the chosen fix.

---

## 3. N3 — `StringEquals` did NOT have the hole. E18-1's diagnosis was wrong about this door

E18-1 §1.4's table says "**no `instanceof String` test on EITHER side**" and
N3 asks for the native's class-id compare to be mirrored into the JIT. **The
JIT already had it**, and has had it since the intrinsic was written.

`jit/src/x64/bytecode_walk.rs:7764-7771`, in the `StringEquals` body, between
the identity check and any field read:

```text
// Class-id check: ObjectHeader.class_id is the i32 at offset 0. `this` is
// a String, so [RAX] is String's class id; a differing [RDX] means a
// non-String argument -> deopt.
// MOV ECX,[RAX] ; CMP ECX,[RDX]
bail.push(self.emit_jcc_rel32_patch(0x85)); // JNE
```

`jit/src/lib.rs`'s own enum comment at `:7609` said so too — "deopts to native
on a coder mismatch or a **non-String argument**". The evidence was in this
lane's file the whole time.

What misled E18-1 is the *other* comment in this file, the one at the
recognition site, which claimed "only null receiver / null argument / null
backing array route to the deopt stub". That was an incomplete list — the
emitted body has two further deopt edges (class-id mismatch, coder mismatch) —
and E18-1 read the comment rather than the codegen. `[window≠absence]`. That
comment is this lane's file and is now corrected and made specific.

So the tier history of `"abc".equals(new StringBuilder(3).append("abc"))` is:

| | before E18-1 | after E18-1 + E27 |
|---|---|---|
| interpreted | `true` (native read `value` by slot) | `false` |
| compiled | class-id mismatch -> deopt -> the same native -> `true` | deopt -> `false` |

The JIT never produced the wrong answer independently; it **inherited** it by
deferring. Both tiers agreed on `true`, which is precisely why no test that
diffs the tiers against each other could see it — E18-1 §1.4's own point,
which applies more sharply than it realised. **No behavioural edit was needed
at this door, and this lane made none.** Measured oracle, for the record:

```text
sb3.length()/capacity()      = 3/3           the builder really does have capacity 3
"abc".equals((Object) sb3)   = false
"abc".contentEquals(sb3)     = true          the sibling that IS supposed to say true
"abc".equals(null)           = false
"abc".equals(new char[]{...})= false
```

### 3.1 The flagged regression risk: verified nil

E18-1 §6 flags "any in-tree caller that was relying on a String matching a
same-length `StringBuilder` now stops matching … it is what a regression looks
like from the outside". This lane grepped for it rather than assuming, over
all `*.java` in the tree. Every hit is safe:

| site | why it is safe |
|---|---|
| `vm/tests/resources/cratonvm/TckStringBuilder.java:4-16` (10 rows) | every one calls `.toString()` first — String vs String |
| `regression-suite/src/RSimpleDateFormatZone.java:270` | a **false positive of my own grep**: `sb` there is `String sb = fmt(b, …)`, a String variable that merely happens to be named `sb` |
| `probes/ShadowDifferentialProbe.java:1156`, `probes/StringDroppedNativesProbe.java:120` | both sides `.toString()`d |
| `probes/LocaleScriptProbe.java:66-68` | `Locale.equals(Locale)`, not String |
| `probes/PrintStreamAppendProbe.java:94`, `ShadowDifferentialProbe.java:1076` | `contentEquals`, whose answer is `true` and does not change |
| `probes/StringPolicyMatrixProbe.java:303` | the one deliberate `PLAIN.equals(new StringBuilder(PLAIN))`. It **emits** the value for live differential comparison against HotSpot rather than asserting it, and no baseline file pins it (grepped). Note it would have answered `false` even before the fix: `new StringBuilder(PLAIN)` has capacity `length + 16`, so the old length check already rejected it. The E18-1 `new StringBuilder(3)` case is the only shape that triggered the bug. |

**No in-tree caller depends on the old answer.** The regression risk is real in
principle and empty in this tree.

### 3.2 One thing this lane noticed and did not fix — flagged, not asserted

The `StringEquals` emitted body has **no null check on the receiver**, while
both its siblings do (`compareTo` at `bytecode_walk.rs:7914`, `indexOf(I)` at
`:8057`, each commented "RAX = this; null receiver -> deopt"). In the `equals`
sequence the receiver is loaded and then dereferenced at `MOV ECX,[RAX]`
(`:7769`) with only `other`-null and identity tests in between.

If reachable, that is `((String) null).equals(x)` returning `false` when `x` is
null (HotSpot: NullPointerException) and dereferencing address 0 when `x` is
not. **This lane did not verify whether an implicit null check covers it** —
an implicit-null-check-via-signal design would make it a non-issue, and this
file is not this lane's. Reported as an observation with exact line numbers so
the owning lane can settle it, not as a defect claim.

---

## 4. What should flip

* **`--only=strfmt`** — no row should move. `strfmt` calls no `toLowerCase`,
  no `indexOf(int)`, and every `.equals(` in it is String-vs-String.
* **`--only=bounds`** — nothing from this lane. E18-1's one predicted flip
  stands on its own.
* **`jit/tests/intrinsic_string_search.rs`** — three tests went red until
  N2a landed (2026-08-18); they were red on `dev` in the interim: `string_search_compare_and_index_of_register_with_a_layout`
  (`:358`), `string_index_of_char_differential` (`:558`),
  `string_index_of_char_null_receiver_deopts` (`:595`). Two of the three
  assert a measurably wrong answer (§2.3).
* **`jit/tests/intrinsic_string_access.rs:709`** — unaffected. It asserts
  `CharSequence.indexOf(I)` is **not** intrinsified, which is still true.

The changes that alter an answer for a valid input are, exhaustively:

* `toLowerCase((Locale) null)` / `toUpperCase((Locale) null)` in compiled
  code — now NPE, matching the interpreter and the oracle (§1);
* `indexOf(int)` at a JIT'd call site for a negative, supplementary,
  out-of-range or invalid `ch` — now the native's answer, which is the JDK's
  (§2). For `0 <= ch <= 0xFFFF` the answer is byte-for-byte what the intrinsic
  produced, so no BMP call site moves.

---

## 5. NOMINATIONS

### N2a — **DONE 2026-08-18.** `jit/tests/intrinsic_string_search.rs`

Landed as written: (a), (b) and (c) below, verbatim. `cargo test --release -p
cratonvm-jit` is now green in full — 15 test binaries, 0 failures;
`intrinsic_string_search` is 14/14.

The three tests had been red on `dev` since the retirement commit
(`9781e456e`), which changed `jit/src/lib.rs` and nothing else — this section
was written, and then not applied. Worth noting for the next lane that writes
a nomination it expects someone else to land: **the prediction in §4 was
exactly right, down to the test names, and that did not make it happen.**

One thing was added beyond (a)-(c): the dead codegen at
`x64/bytecode_walk.rs` still carried the sentence this page exists to refute —
*"Bit-identical to `native_string_index_of`, which likewise masks the
argument"* — both halves false, in the file N2b will open. §2.3's own finding
is that dead code reading as a working implementation is how four copies of
this rule survived, so the comment now says it is dead, why the old claim was
wrong, and what N2b would take.

Three tests pinned the retired intrinsic. Two of them also froze the masking
divergence (§2.3), so this was a correction, not just an accommodation.

**(a)** At `:361-365`, drop the retired row from the loop. Replace exactly:

```rust
    for &(name, desc) in &[
        ("compareTo", "(Ljava/lang/String;)I"),
        ("indexOf", "(I)I"),
        ("indexOf", "(Ljava/lang/String;)I"),
    ] {
```

with:

```rust
    for &(name, desc) in &[
        ("compareTo", "(Ljava/lang/String;)I"),
        ("indexOf", "(Ljava/lang/String;)I"),
    ] {
```

and rename the test at `:358` from
`string_search_compare_and_index_of_register_with_a_layout` to
`string_search_compare_and_index_of_str_register_with_a_layout`.

**(b)** Add, next to it, the row that keeps the retirement honest — without
it, nothing states that the absence is deliberate and the next reader re-adds
it (`[static inertness]`):

```rust
#[test]
fn string_index_of_char_is_not_intrinsified() {
    // `indexOf(I)` is deliberately NOT intrinsified: the inline body masks the
    // needle to `ch & 0xFFFF` and the JDK does not. The gate is
    // `Character.isValidCodePoint`, applied BEFORE any narrowing, and a
    // supplementary `ch` matches the surrogate PAIR. Measured on OpenJDK
    // 25.0.3+9: `"abc".indexOf(0x10061)` is -1, and `"\u{FFFF}q".indexOf(-1)`
    // is -1 even though `(char) -1 == 0xFFFF` and the receiver holds 0xFFFF.
    // The rule lives once, in `lang_string.rs`'s `code_point_needle`; this
    // door reaches it through ordinary dispatch rather than owning a copy.
    // See docs/known-issues/jdk-only/
    // E27-1-the-jit-indexof-int-intrinsic-was-the-fifth-copy.md
    assert!(
        try_resolve_string_intrinsic("java/lang/String", "indexOf", "(I)I", Some(string_layout()))
            .is_none(),
        "indexOf(I) must NOT be intrinsified — the inline body masks to (ch & 0xFFFF)",
    );
}
```

**(c)** Delete `fn compile_index_of_char()` (`:430-475`) and the two tests that
use it, `string_index_of_char_differential` (`:557-593`) and
`string_index_of_char_null_receiver_deopts` (`:595-602`). The first is the one
whose needle list contains `0x1_0000 + ('a' as i32)` with `(ch & 0xFFFF)` as
its expected value; it asserts the wrong answer and cannot be kept.

If the fast path is later restored under N2b's screen, (a)/(b)/(c) invert and
the differential test comes back **with its needle list derived from the
measured JDK rows in §2.1**, not from `& 0xFFFF`.

### N2b — OPTIONAL, restores the throughput. `jit/src/x64/bytecode_walk.rs:8029-8082`

To bring the inline scan back without a copy of the rule: keep the masking
scan but reach it only through a runtime screen, and route everything else to
the existing deopt stub, which already reaches the correct native.

After the receiver / `value` null-check bails and before the needle is
computed, screen the argument. The existing `bail` vector and deopt stub are
the whole mechanism; no new one is needed:

```text
    load ch into a scratch register
    CMP  ch, 0          ; JL   -> bail   (ch < 0: isValidCodePoint fails)
    CMP  ch, 0xFFFF     ; JG   -> bail   (supplementary, or > 0x10FFFF)
```

Inside `0 <= ch <= 0xFFFF` the existing `AND r9d, 0xFFFF` is the identity and
the single-code-unit scan is the complete JDK answer — including for a lone
surrogate. Outside it, the deopt hands the call to `code_point_needle`. That
is a range screen, not a fifth implementation of the rule.

The comment at `:8029-8038` must change either way; it currently states the
masking is "bit-identical to `native_string_index_of`, which likewise masks",
and both halves are now false.

### N2c — `native-builtins/src/lib.rs:8174-8216`: delete two dead, wrong copies

The `indexOf(I)I` and `lastIndexOf(I)I` closures (§2.3). They answer in no
mode, and they are wrong in a way the other copies were not — `chars()` is a
code-point index, so `MIXED.indexOf('z')` would be `4` where HotSpot says `5`.
Delete both `registry.register(...)` calls and leave a note in the style of
the `codePointAt` retirement, naming why: the rule lives in
`lang_string.rs`'s `code_point_needle`, `register_synthetic_overrides`
overwrites these anyway, and in real-JDK mode a `Bridge`-kind `java/lang/String`
native is dropped at registration so the real JDK bytecode answers.

Low urgency (no live wrong answer), high value (`[1 of 10 callsites]` — these
read as working implementations, which is how five copies survived).

### N3-obs — `jit/src/x64/bytecode_walk.rs:7745-7771`: is the `equals` receiver null-checked?

§3.2. An observation to settle, not a defect claim. `StringEquals` is the only
STRING_SEARCH intrinsic with no receiver null test before a dereference of the
receiver; `compareTo` (`:7914`) and `indexOf(I)` (`:8057`) both have one.

### Still open from E18-1, untouched by this lane

N4 (`phases_early.rs` duplicate `getBytes`), N5 (`charset.rs` null charset
name), N6 (`RJdkIntrinsics2.java` search-family rows) and E18-1 N3's fixture
rows all stand as written. The N3 fixture rows are still worth adding **even
though the JIT needed no change** — §3 shows both tiers now answer `false`,
and nothing in the fixture asserts it.

---

## 6. What this lane did NOT do

* **Did not touch `native-builtins/*`, `jit/src/x64/*`, `jit-api/*`,
  `phases_early.rs`, `charset.rs`, `jit/tests/*` or `regression-suite/*`.**
  Read `lang_string.rs`, `bytecode_walk.rs`, `native-builtins/src/lib.rs` and
  both `jit/tests/intrinsic_string_*.rs`; every edit to them is a nomination
  above.
* **Did not write a fifth implementation of the `indexOf(int)` rule.** The
  point of E18-1 §2 was to get from four to one; adding a JIT-side copy would
  have gone back to two.
* **Did not build or run anything.** No `cargo`, no CratonVM binary. Files
  parse-checked with `rustfmt --check` only (§0).
* **Did not take E18-1's N3 on trust.** It was the one nomination of the three
  that turned out to be a wrong diagnosis rather than a stale one, and the
  contradicting evidence was a comment in this lane's own file (§3).
