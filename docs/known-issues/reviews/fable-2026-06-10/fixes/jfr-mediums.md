# Fix note — `jfr-mediums`

Owned files: `jfr/src/builtin.rs`, `jfr/src/dump.rs`, `jfr/src/reader.rs`.

**Note on `reader.rs`:** `jfr/src/reader.rs` **does not exist** in this crate.
The JFR binary reader (`read_events`, `parse_checkpoint_pool`,
`decode_event_value`, `read_jfr_header`) all lives in `jfr/src/dump.rs`. The B1
finding (and S2/S3 metadata docs) therefore landed in `dump.rs`, which is one of
my owned files. No edit was needed or possible in a non-existent file.

## B1 (low/medium) — `read_events` field decode now bounded by the record size — FIXED

`dump.rs` `read_events`, per-record field loop (was `dump.rs:1334-1339`).

The field loop called `decode_event_value(&data, rpos, …)` against the **full
file slice**. The `record_end > events_end` guard only validates the record's
*declared* `total_size`; it does not guarantee that the fields decoded for that
record stay within `[pos, record_end)`. A malformed record could declare an
honest small `total_size` yet contain a tag-3 inline string whose declared
length runs past `record_end` into the next record's bytes — satisfied by those
neighbouring bytes, producing a **silently-wrong decoded value** (no crash, since
all decodes were still bounded by `data.len()` and `pos = record_end` re-syncs
each iteration). This was also inconsistent with `parse_checkpoint_pool`, which
already bounds every decode against `record_end`.

Fix: introduce `let record_slice = &data[..record_end];` and decode every field
against `record_slice` instead of `data`. `record_end <= events_end <=
data.len()` (all validated earlier), so the slice is in range; `rpos` starts at
`pos + size_len <= record_end` and only advances by bytes a `record_end`-bounded
decode consumed — so a field can no longer cross the record boundary. The
`decode_event_value` signature is unchanged (still `data: &[u8]`); only the slice
passed in is tightened. This is strictly tighter than before and cannot break a
well-formed record (whose fields all sit within `[pos, record_end)`).

Added regression test `test_read_events_field_decode_bounded_by_record`: crafts a
one-`string`-field event type and a single record whose declared `total_size` is
honest but whose tag-3 inline string declares length 64 with no in-record
payload, followed by 64 trailing valid-UTF-8 bytes living *past* `events_end`.
Pre-fix the unbounded decode read those trailing bytes and returned `Ok` with a
wrong value; post-fix the bounded decode returns `Err` ("string body
truncated"). The test asserts `is_err()`.

## S1 (medium) — `JfrProfile` / `JfrEventSetting` documented as experimental/not-yet-wired — FIXED (documented)

`builtin.rs` `JfrProfile`, `JfrEventSetting`, `JfrProfile::apply_to`.

Confirmed the finding via workspace grep: the only non-test references to
`JfrProfile` / `default_profile` / `detailed_profile` / `apply_to` are inside the
jfr crate itself; the one hit in `native-builtins/src/spring_startup_bootstrap.rs`
is an unrelated Spring `Environment.getDefaultProfiles()` native, not a
`JfrProfile` consumer. `apply_to` exists but its only caller is this crate's own
`t6_profile_apply_to_settings` test — no `FlightRecorder` startup path applies a
profile, so `enabled`/`threshold`/`stacktrace`/`period` never gate any recording.

Wiring `FlightRecorder` to apply a profile would require editing
`jfr/src/recording.rs`, which is **not an owned file**, so per policy I did not
do that. Instead, per the task's stated preference ("Prefer documenting-as-
experimental over deletion if they are a planned surface"), I added explicit
`EXPERIMENTAL / NOT-YET-WIRED` doc comments to `JfrProfile`, `JfrEventSetting`,
and `apply_to`, stating: nothing in production consults a profile; only the test
suite calls `apply_to`; `apply_to` maps only `enabled` + `threshold`; and the
`stacktrace` / `period` fields are inert (stack-trace capture and periodic
sampling unimplemented). The types are already `pub`, so no `#[allow(dead_code)]`
is needed — the goal was to stop them being mistaken for a working feature.

**Follow-up (not in scope here, owner of `recording.rs`):** wire
`JfrProfile::apply_to` (or a `to_recording_settings`) through
`FlightRecorder`'s recording-startup path so profiles actually gate events.

## S2 / S3 (low) — honest limitation docs on the metadata writer — FIXED (documented)

`dump.rs` `write_metadata_section`.

Expanded the function doc comment with two explicit `LIMITATION` notes:

- **S3:** the metadata section is a *simplified custom encoding*, not stock JFR
  binary metadata. Files round-trip through this crate's own `read_events` but
  are **not loadable by stock JMC / the `jfr` CLI** as fully-described types.
- **S2:** the per-type `has_stacktrace` flag is written faithfully (~20 built-in
  types set it `true`), but **no stack trace is ever captured or serialized** —
  `EventInstance` has no stack-trace representation and `jdk.ExecutionSample`
  carries a caller-supplied pre-formatted `stackTrace` *string field*, not a JFR
  `stackTrace` constant-pool reference. JMC call-tree views would be empty.

The `JfrProfile` doc (S1) cross-references this stack-trace gap.

## Not addressed (out of scope / not owned)

- B2, B3, V1, V2, P1–P3 are not in my task scope.
- Actually wiring profiles / real stack-trace capture / JMC-loadable metadata
  (Feature Suggestions 1–3) require `recording.rs` / `event.rs` and a larger
  design change — documented as follow-ups, not implemented.

## Compile confidence

High. Changes are: one slice-narrowing in the `read_events` field loop (signature
unchanged, strictly tighter bound), three doc-comment expansions (no code
semantics), and one self-contained `#[cfg(test)]` regression test using only
helpers already in scope via `use super::*;` (`write_compressed_int_into`,
`write_compressed_long_into`, `compressed_int_len`, `make_header_with_offsets`,
`EventField`/`EventType`/`EventPeriod`/`EventTypeId`/`EventValue`, all already
imported/defined in the test module). No feature-gated paths touched, so default,
app-stubs, and synthetic-jdk configs are unaffected.
