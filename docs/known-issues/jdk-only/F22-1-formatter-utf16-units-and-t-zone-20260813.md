# F22-1 — `java.util.Formatter` carried its output as a Rust `String`, and `%t` had no time zone

**Date:** 2026-08-13
**Lane:** F22 (`--jdk-only` pool)
**Files:** `native-builtins/src/lang_string.rs`, `native-api/src/registry.rs`,
`vm/src/vm/vm_object.rs`
**Oracle:** HotSpot 25.0.3+9-LTS. Every expectation below was measured before it
was written. Nothing in this record is a predicted *behaviour*; the only
predicted claims are marked PREDICTED and concern which checks flip.

---

## 1. The false premise this lane was given, and what was actually true

The brief stated: *"F13 reports the lossless reader already exists —
`read_java_string_units` at `vm/src/vm/vm_object.rs:758` — and there is no
writer."* It asked for a new `create_string_from_utf16` on `NativeContext`,
with the trait `impl` handed back as a blocking NOMINATION against
`vm/src/vm/vm_exec.rs` (owned by lane F19).

**The writer exists at every layer, and has for some time.** There are three of
them and they are the *same* code:

| layer | symbol | file |
|---|---|---|
| VM | `create_java_string_from_units` / `try_create_java_string_from_units` | `vm/src/vm/vm_object.rs:220` |
| VM | `populate_java_string_fields` | `vm/src/vm/vm_object.rs` (shared core) |
| trait | `NativeContext::init_string_from_units` | `native-api/src/registry.rs:2807` |
| trait impl | `VmNativeContext::init_string_from_units` -> `populate_java_string_fields` | `vm/src/vm/vm_exec.rs:11698` |
| natives | `lang_string::sb_string_from_units` | `native-builtins/src/lang_string.rs:2355` |

`create_java_string_from_units` and `init_string_from_units` bottom out in the
*same* `populate_java_string_fields`, so there is exactly ONE encoding of a
compact `String`'s coder and byte order, and the native side already reaches it.
`sb_string_from_units` is that pair packaged with a GC pin and a well-formed
fast path — and it had **three callers**, all in `StringBuilder`/`substring`
territory, which is why it read as absent from the formatter's side of the file.
This is the "a correct helper already exists with one caller — grep the SHAPE"
pattern, in its most expensive form: three separate arms were annotated as
blocked on a primitive that was one function call away.

**Consequences.**

* **The `vm_exec.rs` NOMINATION is withdrawn. It is NOT blocking — it is
  unnecessary.** No edit to `vm/src/vm/vm_exec.rs` is needed for this work, and
  none was made.
* No new trait method was added. A `create_string_from_utf16` would have been a
  *third* spelling of one concept, reconciled only where the bytes are consumed
  — which is exactly the failure mode the coder/endianness agreement is
  vulnerable to.
* Instead, the reader and the writer were wired together **in documentation**,
  which is what the brief actually wanted ("so the two are maintained
  together"): `read_java_string_units` now names its write-side twin and the
  full native-reachable path, and `init_string_from_units` now says in so many
  words that it *is* the lossless writer and that a fourth one must not be
  added. Those are the two owned-file edits outside `lang_string.rs`.

A second brief claim was also already false: *"the upper-caser ran after the
width justifier"*. It does not — that was fixed before this lane started, and
the fix is documented in place at the `uppercase_result` step. It was
**verified**, not re-fixed: `String.format(ROOT, "%5S", "ß")` is `"   SS"`,
5 units, and the ordering is now additionally covered by a test.

---

## 2. Measured HotSpot 25 rows

Dumped as `length()` plus hex code units.

### 2.1 Surrogates through the formatter

| expression | HotSpot 25 | CratonVM before |
|---|---|---|
| `format(ROOT,"%.1s","😀")` | len 1 — `D83D` | len 0 (stopped before the pair) |
| `format(ROOT,"[%5.1s]","😀")` | len **7** — `005B 0020 0020 0020 0020 D83D 005D` | len 7 but `005B 0020*5 005D` — wrong units |
| `format(ROOT,"%.3s","a😀b")` | len 3 — `0061 D83D DE00` | as HotSpot (already fixed) |
| `format(ROOT,"%s","x\uD800y")` | len 3 — `0078 D800 0079` | `0078 FFFD 0079` |
| `format(ROOT,"%c",(int)0xD800)` | len 1 — `D800` | `003F` (`'?'`) |
| `format(ROOT,"%.1S","😀")` | len 1 — `D83D` | len 0 |
| `format(ROOT,"%.2s","\uD800\uD800\uD800")` | len 2 — `D800 D800` | `FFFD FFFD` |
| `"a\uD800b".repeat(2)` | `0061 D800 0062 0061 D800 0062` | `0061 FFFD 0062 0061 FFFD 0062` |
| `"\uDC00".repeat(3)` | `DC00 DC00 DC00` | `FFFD FFFD FFFD` |

Note how the `[%5.1s]` row hides itself: **both** wrong truncators produce
length 7. A test that asserts only the length passes against a broken
implementation. The test added here asserts the units.

### 2.2 Truncate-vs-uppercase order (the upper-caser can GROW past the precision)

| expression | HotSpot 25 |
|---|---|
| `format(ROOT,"%.1S","ß")` | len 2 — `0053 0053` |
| `format(ROOT,"%.2S","ßx")` | len 3 — `0053 0053 0058` |
| `format(ROOT,"%5.1S","ß")` | len 5 — `0020 0020 0020 0053 0053` |
| `format(ROOT,"%.2S","\uD800ß")` | len 3 — `D800 0053 0053` |

So the order is **truncate, then upper-case, then justify** — `print(Formatter,
String, Locale)`'s own order. Mid-edit this lane briefly reordered the null path
to upper-case first; the measurement above caught it and it was reverted. An
ASCII-only row cannot tell the two orders apart, which is how this stays
invisible.

### 2.3 `%t` time zone — `new Date(1699999999000L)`, `Locale.ROOT` unless noted

| `user.timezone` | `%tc` | `%tZ` | `%tz` | `%tH` | `%ts` |
|---|---|---|---|---|---|
| `UTC` | `Tue Nov 14 22:13:19 UTC 2023` | `UTC` | `+0000` | `22` | `1699999999` |
| `America/New_York` | `Tue Nov 14 17:13:19 GMT-05:00 2023` | `GMT-05:00` | `-0500` | `17` | `1699999999` |
| `Asia/Kolkata` | `Wed Nov **15** 03:43:19 GMT+05:30 2023` | `GMT+05:30` | `+0530` | `03` | `1699999999` |
| `Europe/Berlin` | `Tue Nov 14 23:13:19 GMT+01:00 2023` | `GMT+01:00` | `+0100` | `22`->`23` | `1699999999` |

Under `Locale.US` the same `%tZ` cells read `EST`, `IST`, `CET`; in July (DST),
`EDT` / `GMT-04:00` and `CEST` / `GMT+02:00`. A `Calendar` argument uses **its
own** zone regardless of the default: `Calendar.getInstance(getTimeZone("Asia/
Tokyo"))` at the same instant gives `Wed Nov 15 07:13:19 GMT+09:00 2023`.

`%ts` / `%tQ` do **not** move with the zone — they are epoch quantities.

**The brief framed this as "`%tc` and `%tZ` hard-code `UTC`". That is the
visible half of a larger defect.** `extract_temporal_fields`' `millis_to_fields`
divided the epoch millis directly with **no zone offset at all**, so every `%t`
field — hour, day, month, year — was UTC. The hard-coded `"UTC"` / `"+0000"`
were *consistent with* that, not the cause of it. The Kolkata row rolls the
**date** over, so naming the zone alone could not have fixed `%tc`.

JDK source confirms the mechanism (`java.util.Formatter.printDateTime`):
`long`/`Date` build `Calendar.getInstance(l == null ? Locale.US : l)` — which
carries `TimeZone.getDefault()` — and `%tZ` is
`tz.getDisplayName(DST_OFFSET != 0, TimeZone.SHORT, requireNonNullElse(l, Locale.US))`.

### 2.4 `java.time` sources — measured, deliberately NOT changed

| argument | `%tZ` on HotSpot 25 |
|---|---|
| `Instant` | **throws** `IllegalFormatConversionException: Z != java.time.Instant` |
| `LocalDateTime` | **throws** `IllegalFormatConversionException: Z != java.time.LocalDateTime` |
| `ZonedDateTime` (Tokyo) | `GMT+09:00`, `%tz` `+0900`, `%tH` `07` |
| `OffsetDateTime` (-3) | `-03:00`, `%tz` `-0300`, `%tH` `19` |

---

## 3. What changed

All in `native-builtins/src/lang_string.rs` unless stated.

### 3.1 The formatter now carries its output as UTF-16 code units

The pipeline was `String`-shaped end to end, so the *last* step always destroyed
what the earlier ones preserved.

* `format_impl`'s accumulator is `Vec<u16>`; it ends in `sb_string_from_units`.
  For a well-formed result that is still exactly the previous
  `create_string_uninterned_gc_safe` path — no allocation, interning or
  GC-safety property moves — and only a result that actually carries an unpaired
  surrogate takes the units branch.
* The format string is read with `read_string_chars`, not `read_string`. It was
  lossy too: literal runs between specifiers are copied verbatim, so a lone
  surrogate in the format string became U+FFFD before parsing.
* The parser's `chars` view is now **one `char` per code UNIT** (was one per
  code point) and index-aligned with the units, so the literal branch pushes the
  raw unit. The parser is unaffected because every character it tests for is
  ASCII.
* `format_arg_full` and `format_temporal_field` return `Vec<u16>`.
* `fmt_pad_to_width` takes and returns units — the one justifier, unchanged in
  count. `fmt_pad_str_to_width` is a one-line adapter for the numeric and `%t`
  paths, whose output is ASCII/`DateFormatSymbols` text by construction.
* `fmt_truncate_utf16` (a `char_indices` walk) is replaced by
  `fmt_truncate_units` (a `Vec::truncate`).
* New `fmt_upper_case_units`: splits on surrogates that cannot participate in a
  case mapping, maps each well-formed run through the existing `fmt_upper_case`
  (so `case_map`'s `tr`/`az`/`lt` rules are untouched), and carries lone
  surrogates through verbatim.

### 3.2 The general family is split out, which is also the `%s` fast path

`'s'`/`'b'`/`'h'`/`'c'` now take a dedicated branch — truncate, upper-case,
justify — because every later step is inapplicable to them by the JDK's own
structure. The raw producer is the new `fmt_general_units`:

* `%s` of a `java/lang/String` reads `read_string_chars` directly, using the
  class-identity comparison that **already existed** further down the file (in
  `format_arg`'s own `%s` arm) — **hoisted, not duplicated**.
* This is F13's suggested `%s` fast path: a `java/lang/String` argument no
  longer reaches `fmt_formattable_dispatch`'s interface probe. **The speedup is
  UNMEASURED and is claimed as nothing.** It is behaviour-identical because
  `java.lang.String` is final and does not implement `java.util.Formattable`, so
  the skipped probe could only ever have answered "no". It was taken because it
  is the lossless path anyway.
* `%c` re-renders from the code point via `fmt_char_code_point`.
  `format_arg` still runs **first** and owns every refusal
  (`IllegalFormatCodePointException`, `IllegalFormatConversionException`), so
  the two cannot drift on which code points are legal; a range guard falls back
  to `format_arg`'s rendering if they ever disagree about unboxing.

### 3.3 `native_string_repeat`

Repeats the code units and ends in `sb_string_from_units`. The fallible
reservation now measures the `value` array — the same quantity the JDK's own
overflow guard directly above it measures — instead of a UTF-8 length.

### 3.4 `%t` gets a real time zone

* New `FmtZone { offset_ms, dst, known }`, returned alongside the fields by
  `extract_temporal_fields`.
* `long`/`Long`/`Date` resolve `TimeZone.getDefault()`; a `Calendar` resolves
  `cal.getTimeZone()`. Offset is `tz.getOffset(millis)`; `dst` is
  `getOffset(millis) != getRawOffset()`, i.e. `DST_OFFSET != 0`.
* The fields are computed from `millis + offset`, so hour/day/month/year are
  local. `%ts` / `%tQ` subtract the offset back out.
* `%tZ` and `%tc`'s zone slot call
  `tz.getDisplayName(dst, TimeZone.SHORT, locale)`, falling back to the
  `GMT±HH:MM` form (which is HotSpot's own `Locale.ROOT` answer).
* `%tz` renders `ZONE_OFFSET + DST_OFFSET` as `{-|+}HHMM`.
* **`known == false` preserves the old behaviour exactly** (offset 0, `"UTC"`,
  `"+0000"`), so a VM that cannot reach `java.util.TimeZone` does not regress
  and does not start printing `GMT+00:00` where it printed `UTC`.

The `TimeZone` object is **re-resolved at point of use** rather than held in
`FmtZone`: `%tc` runs three `fmt_date_name` bytecode invocations between the
field extraction and the zone-name lookup, and a reference held across those is
a moved-object hazard.

---

## 4. Verified vs. assumed

**Verified (measured or read in source):**

* Every row in section 2, on HotSpot 25.0.3+9-LTS.
* The truncate-before-uppercase order, against `java.util.Formatter`'s
  `print(Formatter, String, Locale)` in `C:\craton\jdk25src`.
* `%tZ`'s exact expression, in `Formatter.java`'s `DateTime.ZONE` case.
* `%tc`'s composition (`a`, `b`, `d`, `T`, `Z`, `Y`), in its `DATE_TIME` case.
* That `init_string_from_units` and `create_java_string_from_units` share
  `populate_java_string_fields` — read in `vm_object.rs` / `vm_exec.rs`.
* That the upper-caser already ran before the justifier (brief claim was stale).
* All three owned files parse (`rustfmt` on a copy) and the two blocks rustfmt
  wanted reflowed were reflowed.

**Assumed / PREDICTED (not measured — no build or VM run was performed by this
lane, per its constraints):**

* That the crate compiles. The changes are type-directed and every call site of
  every changed signature was updated, but **this is unverified by a compiler.**
  The single caller of `format_arg_full` and the single caller of
  `format_temporal_field` were both updated; `fmt_pad_to_width` has 4 call sites
  and `extract_temporal_fields` 1, all updated.
* PREDICTED: the surrogate rows in section 2.1 flip from wrong to correct.
* PREDICTED: the `%t` zone rows in 2.3 flip on a non-UTC host. **On a UTC host
  every one of them is already green and will stay green — this change is
  invisible there.** Anything validating it must set `-Duser.timezone`.
* PREDICTED: nothing else moves. The well-formed path is byte-identical by
  construction (`sb_string_from_units` early-returns to the old call for any
  slice with no unpaired surrogate), and the numeric conversions never leave the
  `String` shape.

**Reachable, not flipped.** `sb_string_from_units`' lossless branch was already
correct and already tested; what changed is that the formatter and
`String.repeat` now *reach* it. No new correctness was added to the writer
itself.

---

## 5. NOMINATIONS

### 5.1 WITHDRAWN — `vm/src/vm/vm_exec.rs` `create_string_from_utf16` impl

**Not blocking, and not needed.** See section 1. No edit to `vm_exec.rs` is
required by this work and none was made. F19 is unaffected.

### 5.2 OPEN — `java.time` sources have no zone, and two of them should refuse

`extract_temporal_fields` returns `FmtZone::NONE` for `Instant`, `LocalDate`,
`LocalTime`, `LocalDateTime`, `ZonedDateTime`, `OffsetDateTime`. Measured
(section 2.4):

* `Instant` and `LocalDateTime` must **throw** `IllegalFormatConversionException`
  for `%tZ`/`%tz`; they currently answer `UTC`/`+0000`.
* `ZonedDateTime`/`OffsetDateTime` carry a real offset that `%tZ`/`%tz` should
  report; it comes off the object (`getOffset()`), not off a `TimeZone`, so it
  is a different lookup from the one added here.

Left out deliberately: adding a refusal is a behaviour change with its own blast
radius, and the offset lookup is a separate mechanism. Both are cheap follow-ups
now that `FmtZone` exists.

### 5.3 OPEN — `Given(None)` locale for `%t` name lookups

`fmt_date_name` and the new `fmt_zone_display_name` both resolve an explicit
**null** locale to the DEFAULT locale. The JDK source says
`Objects.requireNonNullElse(l, Locale.US)` for both. This predates this lane,
is shared by both helpers (so they do not disagree with each other), and is
**not observable on a host whose default locale is `en_US`** — which is why it
is recorded rather than guessed at. It needs a host with a non-US default
locale to measure.

Note this is a *third* answer, distinct from the two the brief warned about:
`fmt_upper_case` takes the default locale for a null, `fmt_symbols_for` takes no
localization at all. Those two were read and left alone.

### 5.4 OPEN — a `Formattable` that emits a lone surrogate is still lossy

`fmt_formattable_dispatch` returns a Rust `String` because the callee's output
is collected with `read_string`. The loss is at that READ, not in the pipeline.
One `read_string` -> `read_string_chars` change plus a `Vec<u16>` return type.

---

## 6. Left undone

* No build, no test run, no VM execution — lane constraint.
* The `Formattable` read path (5.4) and the `java.time` zones (5.2).
* `fmt_utf16_len` survives for the numeric zero-pad step, which is still
  `String`-shaped. That is correct (numeric output is ASCII) but it does mean
  the file still has two length notions; the units one is now the default.
