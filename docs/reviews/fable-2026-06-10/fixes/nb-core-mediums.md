# Fix note: nb-core-mediums

Agent: nb-core-mediums (fable-2026-06-10 deferred medium findings)
Owned files: `native-builtins/src/lib.rs`, `native-builtins/src/phases_early.rs`
Report: `docs/reviews/fable-2026-06-10/nb-core.md` (B2, B3, B4, B6 dedup, B8)

All edits are in non-cfg-gated code; default / app-stubs / synthetic-jdk configs
are unaffected (no cfg branches added, no cfg balance touched).

---

## B3 — Scanner(InputStream) now reads the wrapped stream (FIXED)

`phases_early.rs` `register_scanner_natives`. Previously `new Scanner(stream)`
stored an empty string, so every `hasNext/next/nextLine/nextInt/...` saw no
data — a silent functional stub.

Fix: the `(Ljava/io/InputStream;)V` ctor now drains the stream into the source
buffer via a new helper `scanner_drain_input_stream`, which calls
`stream.readAllBytes()` virtually (`ctx.invoke_virtual(stream, "readAllBytes",
"()[B", &[])`) so any concrete stream (FileInputStream, System.in,
ByteArrayInputStream, …) supplies its bytes, then decodes them as UTF-8 (lossy).
Reads the byte[] with `array_length` + `read_byte_array_into` (the bulk
intrinsic). Falls back to an empty string if the stream yields nothing or the
call fails — never panics.

Limitation: the whole stream is drained eagerly at construction (the synthetic
Scanner is buffer-backed, not incremental). UTF-8 decode is lossy; close enough
for the default-charset Scanner model.

## B4 — Scanner UTF-8 char-boundary-safe slicing (FIXED)

`phases_early.rs`. All seven `&source[pos.min(source.len())..]` byte-index
slices (hasNext / hasNextInt / next / nextLine / nextInt / nextLong /
nextDouble) replaced with a new `scanner_remaining(&source, pos)` helper that
snaps `pos` UP to the next UTF-8 char boundary before slicing
(`is_char_boundary` loop), so a stored byte offset that ever lands
mid-codepoint yields a valid slice instead of panicking
("byte index N is not a char boundary"). The happy path (ASCII delimiters →
`pos` already a boundary) is byte-identical to before.

Not touched (out of B4 scope, separate pre-existing bug): `StringReader.read()`
(`phases_early.rs` ~line 2232) slices using the char codepoint value as a byte
length (`&source[ch.min(...)..]`) — wrong logic + can also mis-boundary. Flagged
below, not fixed here.

## B2 — essential_quarkus_locale_convert("all") returns Locale.ROOT (FIXED)

`lib.rs` `essential_quarkus_locale_convert`. The `"all"` case returned
`Locale.getDefault()` (the host locale, e.g. en_US) despite the comment saying
`Locale.ROOT`. Fixed to construct the empty locale `new Locale("", "", "")`
(which is exactly `Locale.ROOT`) via the same `(String,String,String)`
constructor path the function already uses for the general case — avoids a
static-field lookup and matches the existing style.

## B6 dedup — deleted stale SubmissionPublisher block in lib.rs (FIXED)

`lib.rs` `register_t31_concurrent_extras` (~line 21231) registered a duplicate
`SubmissionPublisher` `<init>()V` / `close()V` / `isClosed()Z` block. Because
`register_t31_concurrent_extras` is registered LAST (after
`register_phase60_natives` at the call site lib.rs:9603 vs 9783), these stale
versions SHADOWED the phase-60 (`register_p60_flow`, phases_late.rs:16549+)
implementations that Round 5 / the nb-core-stubs agent wired up for real
subscriber delivery. In particular the stale `close()V` only flipped the closed
flag and never fired `onComplete()`, defeating the fix.

Deleted the three stale registrations (left a breadcrumb comment). Now the
delivering phase-60 versions win:
- phase-60 `<init>` sets field 0 = null; the nb-core-stubs delivery helpers
  (`sp_wrapper_ensure`) lazily create the identical ArrayList-style wrapper on
  first subscribe, so the layout stays consistent — verified against
  phases_late.rs:16388-16474.
- phase-60 `close()` fires `onComplete()` to every subscriber.

This is exactly the "owner action needed in lib.rs" handoff that nb-core-stubs.md
(lines 73-79) requested (option 1: delete the block). The unrelated
`Flow$Publisher.subscribe` and `Flow$Subscription` registrations in the same
function were left untouched (different classes, outside the B6 scope).

## B8 — MessageFormat honors number format elements (PARTIAL, correct-or-plain)

`phases_early.rs` `p52_message_format_apply` / `p52_format_value`. Element
parsing now splits `{idx,type,style}` into 3 parts (`splitn(3, ',')` so a number
subformat may itself contain grouping commas) and dispatches through a new
`p52_format_typed_value`:
- `{n,number}` and `{n,number,STYLE}` are formatted:
  - `integer` → grouped integer
  - `percent` → value×100 grouped + `%`
  - `currency` → `$` + 2-fraction-digit grouped
  - explicit decimal patterns (`#,##0.00`, `0.000`) → fraction-digit count taken
    from the run after `.`, grouping if the pattern contains `,`
  - default (no style) → general number form (grouped, up to 3 fraction digits,
    trailing zeros trimmed)
- `date` / `time` / `choice` are NOT localized (out of scope) — they fall
  through to the plain value rendering rather than being silently dropped, so
  output is at least correct in magnitude.

Also improved `p52_format_value`: non-String reference args now render via
`toString()` (`invoke_virtual`) instead of the misleading literal `"null"`.

New pure helpers: `p52_format_typed_value`, `p52_format_number`,
`p52_format_fixed`, `p52_group_int`, `p52_group_digits`.

Note: number formatting uses a fixed `,`/`.` grouping/decimal convention (US),
not a locale-driven `DecimalFormatSymbols`. Full ICU date/time/choice formatting
remains a documented gap.

---

## Tests added

`phases_early.rs` `t2_tests` module (pure-helper coverage, no mock ctx needed):
- `scanner_remaining_snaps_to_char_boundary` — multi-byte `é`, mid-codepoint
  offset does not panic; boundary + past-end behavior (B4).
- `mf_number_integer_groups_thousands`, `mf_number_percent_and_currency`,
  `mf_number_explicit_decimal_pattern`, `mf_number_default_style_general_form`,
  `mf_group_digits_basic` — MessageFormat number element rendering (B8).

## Not fixed (noted for follow-up)

- `StringReader.read()` byte-index/codepoint slice bug (phases_early.rs ~2232) —
  adjacent to B4 but a distinct logic error; out of the assigned B4 line scope.
- MessageFormat date/time/choice localization and locale-driven number symbols.

## Compile confidence

High. All edits mirror existing patterns in the same files
(`ctx.new_object` + `invoke "<init>"` for B2; `invoke_virtual` + `array_length`
+ `read_byte_array_into` for B3; pure string ops for B4/B8). No new imports
required (`NativeContext`, `ObjectRef`, `Value`, std prelude already in scope).
No `deny(warnings)` in the crate. cargo/git not run per the round rules.
