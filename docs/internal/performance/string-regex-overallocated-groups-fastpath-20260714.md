# String/Regex `Matcher.groups[]` capacity fast-path fix (2026-07-14)

## Status

Fixed on topic commit `471dd417` (`codex/perf-string-regex-20260713-001`) and merged
to `dev` by `71f1516e`. The change restores the existing real-JDK `Matcher` native
fast path for runtime layouts that overallocate the `groups[]` backing array. The 1M
README remeasurement below used later current-dev commit `331b279e`.

## Symptom and root cause

`bench/StringRegexOnly.java` repeatedly executes `Matcher.find()` and `group(1)` over
`value0,value1,...`. At 100K entries, a fresh pre-fix run took a median 2,065 ms in
CratonVM versus 59 ms in HotSpot (35.0x).

The native fast path already supported this pattern, but its safety guard required:

```text
Rust captures_len() == Matcher.groups.length / 2
```

On the real JDK layout loaded by this runtime, `Matcher.groups` is a capacity array:
the one-explicit-group pattern has two semantic captures (whole match plus group 1),
but the array has room for ten capture pairs. The equality therefore rejected every
`find()` and sent the hot loop back through real regex bytecode. A gated diagnostic
probe observed the exact bailout on every call:

```text
find bail: capture count 2 != groups capacity 10
```

## Fix

- Resolve `Pattern.capturingGroupCount` by field name and use it as the authoritative
  semantic count (including group zero).
- Require Rust's compiled regex capture count to equal that semantic count.
- Treat `Matcher.groups.length` only as a minimum capacity bound.
- Validate `start(int)`, `end(int)`, and `group(int)` against the semantic count so
  overallocated slots cannot make invalid group indices legal.
- Preserve the existing bail-to-real-bytecode behavior when fields/layouts cannot be
  resolved or when semantic counts genuinely disagree.

Two pure unit tests pin the regression: a two-capture layout with `groups.length == 20`
is accepted, while semantic-count mismatches, short capacity, negative indices, and
group 2 remain rejected.

## Validation

Targeted Rust test:

```text
cargo test -p cratonvm-native-builtins matcher_realjdk_layout_tests -- --nocapture
test result: ok. 2 passed; 0 failed; 2994 filtered out
```

`MatcherFastPathParity` produced byte-for-byte matching HotSpot output for basic and
optional captures, `groupCount`, invalid group exceptions, `find(int)`, reset/new
input, UTF-16 offsets around a supplementary character, zero-width progression,
regions/anchoring, and transparent-bounds fallback.

Release artifact:

```text
/data/cratonvm-perf-string-regex-artifacts-20260713/
  cratonvm-string-regex-471dd417-20260714-001.bin
SHA-256 38582cda6f3a4bea58137d162b26fe1aac356ed42e10fc13581e8215653e6b97
```

The artifact was built in the unique target directory
`/data/data/cratonvm-perf-string-regex-target-20260714-471dd417`.

The current-dev 1M remeasurement used this separate unique artifact:

```text
/data/cratonvm-perf-string-regex-artifacts-20260713/
  cratonvm-string-regex-1m-331b279e-20260714-001.bin
SHA-256 805ddffd3b9edcc5d9a3030c13f989e6392a87ec3ffb1398f88faebb3a71b74e
```

It was built in
`/data/data/cratonvm-perf-string-regex-1m-target-20260714-331b279e`, with compiler
temporaries redirected to `/data` because the host's shared `/tmp` filesystem was full.

## Fresh 1M benchmark (README row)

Method: Azure Linux benchmark host, logical CPU 14 via `taskset`, freshly launched
process per sample, alternating CratonVM/HotSpot order, nine paired rounds. HotSpot was
Temurin JDK 25.0.3 C2. Harness source SHA-256:
`76e3ae6010061db3da1b366c9e0a4a598d9487e3bf99c9132bdbb1f27bb66fde`.
Every sample returned checksum `500000500000`.

```text
CratonVM: 4841, 4792, 4839, 4784, 4763, 4779, 4785, 4806, 4782 ms
HotSpot:   145,  149,  142,  146,  149,  146,  143,  146,  142 ms
Median:  4785 ms CratonVM / 146 ms HotSpot = 32.8x
```

Compared with the earlier 100K result below, CratonVM time scales almost exactly 10x
while HotSpot time scales 2.5x. The larger 1M ratio therefore exposes the remaining
steady-state throughput gap after process startup and tiering costs are amortized.

## Historical fresh 100K benchmark

Method: Azure Linux benchmark host, logical CPU 14 via `taskset`, freshly launched
process per sample, alternating CratonVM/HotSpot order, nine paired rounds. HotSpot was
Temurin JDK 25.0.3 C2. Harness source SHA-256:
`76e3ae6010061db3da1b366c9e0a4a598d9487e3bf99c9132bdbb1f27bb66fde`.
Every sample returned checksum `5000050000`.

```text
CratonVM: 477, 478, 482, 476, 475, 473, 470, 483, 474 ms
HotSpot:   56,  59,  58,  60,  60,  58,  57,  56,  61 ms
Median:   476 ms CratonVM / 58 ms HotSpot = 8.21x
```

Fresh pre-fix 100K samples from the same harness and CPU were:

```text
CratonVM: 2065, 2046, 2083 ms (median 2065 ms)
HotSpot:    59,   59,   55 ms (median   59 ms)
Ratio: 35.0x
```

The fix reduces CratonVM latency by 77.0% and reduces the ratio gap to parity by 78.8%.

## Steady-state 1M follow-up (2026-07-14)

The first 1M result above exposed fixed work that the 100K run largely hid behind
startup. The follow-up on `codex/perf-string-regex-steady-20260714-001` keeps the same
real-JDK-layout escape hatches while reducing the successful-match path:

- visit capture ranges directly instead of allocating a capture vector per match;
- materialize groups from the already-decoded cached text rather than nested virtual
  substring dispatch;
- retain the decoded text/offset tables/compiled regex in an `Arc`, with a thread-local
  exact-object hit and the stable-identity global map as the cold/GC fallback;
- validate and cache `Pattern.capturingGroupCount` once per exact Pattern, while still
  checking the current `groups[]` capacity;
- guard cached `from`/`to`/`first`/`last` state with JDK 25's `Matcher.modCount`, so
  reset, region, usePattern, and other public match operations force a refresh;
- bulk-create captured ASCII compact Strings while preserving fresh object identity;
- reuse the first StringBuilder state read and bulk-write its char-array append payload.

The final retained release artifact is:

```text
/data/cratonvm-perf-string-regex-artifacts-20260713/
  cratonvm-string-regex-steady-sbbulk-05750b3d-20260714-014.bin
SHA-256 af84d4819b58c6c0250f4b4acd347c97110ada0c2bd4970345123b67c799dca7
```

It was built from runtime commit `05750b3d` in the dedicated target directory
`/data/data/cratonvm-perf-string-regex-steady-target-20260714-39633721-001`.
The later bulk-decoder experiment was performance-neutral and was reverted; the final
branch source for the retained StringBuilder path is byte-for-byte identical to this
artifact's source.

Final method: the same Azure host and `StringRegexOnly 1000000` harness, Temurin JDK
25.0.3 C2, logical CPU 14 via `taskset`, fresh process per sample, alternating launch
order for nine paired rounds. No `rustc` process was active when the set began. Every
sample returned checksum `500000500000`.

```text
CratonVM: 3468, 3475, 3489, 3511, 3422, 3448, 3427, 3428, 3490 ms
HotSpot:   143,  144,  144,  144,  142,  143,  141,  147,  145 ms
Median:  3468 ms CratonVM / 144 ms HotSpot = 24.1x
```

Relative to the prior 1M README median (4,785 ms / 146 ms = 32.8x), CratonVM time is
1,317 ms lower (**27.5%**) and the ratio is down to **24.1x**. The full 15-case
`MatcherFastPathParity` output remained byte-for-byte identical to HotSpot across
basic/optional captures, indexed accessors, reset/new input, `find(int)`, Unicode,
zero-width matches, regions, anchoring, and transparent-bounds fallback. Focused
validation also passed five Matcher layout/cache tests, 21 StringBuilder tests, the
compact-ASCII fresh-identity round-trip test, and the 107-observation interpreter
intrinsic on/off differential.
