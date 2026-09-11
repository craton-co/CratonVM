# String/Regex: the row is LINEAR — it is HotSpot's column that bends

2026-09-11. Azure EPYC bench host, JDK 25.0.4+7 both sides, `bench/StringRegexOnly.java`.

## The premise, checked before anything was optimised

The published scaling table reads as though something in CratonVM were
super-linear:

| n | HotSpot | CratonVM | ratio |
|---|---------|----------|-------|
| 100 K | 54 ms | 242 ms | 4.48x |
| 1 M | 138 ms | 2 320 ms | 16.81x |
| 10 M | 466 ms | 23 359 ms | 50.13x |

It is not. Read the CratonVM column on its own and it is a straight line:
242 → 2 320 → 23 359 is ×9.6 then ×10.1 for ×10 the input each time. The
HotSpot column is the one that bends: 54 → 138 → 466 is ×2.6 then ×3.4,
because C2's fixed warmup is most of the 100 K number and almost none of the
10 M one. **The ratio column measures HotSpot amortising its warmup, not
CratonVM degrading**, and a `perf stat` sweep of n = 100 K … 1.6 M on this host
confirms it: CPU-time per element is flat once the ~1.3 s of fixed VM startup
is subtracted, with no second-order term to find.

So there is no `O(n²)` to delete. Closing the gap means cutting the
per-element constant, and the ratio stops growing on its own once both columns
are in their linear regime.

## Where the constant goes

`perf record` at n = 400 000, split into the two phases with a `SrPhases`
probe (build the string with `StringBuilder`, then the
`find()`/`group()`/`parseLong` loop):

- builder phase ≈ 30 % of the kernel, regex loop ≈ 70 %;
- the profile is FLAT, and it is almost entirely VM plumbing. The regex
  engine itself — `matcher_realjdk_search` plus the `regex` crate's DFA
  searches — is **under 4 % of samples**. Everything else is the native-call
  boundary and validated heap round-trips.

Top self-time, builder phase:

| % | symbol | what it is |
|---|--------|-----------|
| 9.8 | (kernel) | page-fault / allocation |
| 5.4 | `is_object_address` ×2 | the membership walk behind every validated heap access |
| 2.4 | `try_jit_site_cached_native_dispatch` | JIT → native call setup |
| 2.2 | `resolve_field_index_by_class_id` | **by-NAME field lookup, per append** |
| 1.9 | `safe_native_call_impl` | the native boundary |
| 1.6 | `__memcmp_evex_movbe` | **the field-name comparisons that lookup drives** |
| 1.5 | `set_field_by_name` | **a second by-NAME lookup, per append** |
| 1.2 | `coerce_field_value_for_slot` | per-field-write validation |
| 0.7 | `mi_theap_malloc_aligned` + `mi_free` | **two Rust allocations per append** |

## The four wastes

### 1. `count` was resolved by NAME on every `append`

`sb_view` → `sb_raw_count` → `sb_field_slot(ctx, this, "count")` →
`resolve_field_index_by_class_id`, which takes a class-manager **read lock**
and walks the hierarchy comparing field names. `sb_set_count` then did it a
second time on the write side. Two locked name walks per `append`, and
`StringRegexOnly` makes two `append`s per element of its input.

A name → slot answer is a property of the CLASS. `SB_SLOT_MEMO` caches it
keyed by `ClassId`, which makes the answer permanent: a `ClassId` names one
loaded class in one loader, and JVMTI `RedefineClasses` may not change a
class's field layout. (The `java.util.regex` fast path next door already
caches the same kind of answer in a process-global `OnceLock` keyed only by
class NAME; this is the stricter key, not a looser one.)

### 2. `toStringCache` was probed by NAME on every `append`, and always missed

`sb_set_count` guarded its `StringBuffer.toStringCache` invalidation with a
field-COUNT heuristic (`count_slot + 1 < num_fields`) and then did an
unconditional `set_field_by_name`. For a `StringBuilder` that lookup can only
ever miss — the class declares no such field — and a miss is the most
expensive shape the lookup has: it compares every field name in the hierarchy
before concluding. It is now resolved through the same memo, so the negative
answer is computed once per class and the write is skipped outright
afterwards.

This is also strictly closer to what `StringBuffer` specifies: the cache is
cleared when the receiver DECLARES one, rather than when a field-count
heuristic happens to hold.

### 3. `append(int)` / `append(long)` allocated twice per call

`val.to_string()` malloc'd and freed a `String`, and `sb_append_str` then
malloc'd and freed a `Vec<u16>` for the UTF-16 encoding. Both are gone: a
20-byte stack buffer renders the digits (`sb_render_signed`), and
`sb_append_str` encodes text of 64 bytes or less into a stack `[u16; 64]`.
A UTF-16 encoding is never longer in units than the UTF-8 is in bytes, so
`text.len()` is a sound bound on the unit count.

### 4. `Matcher.find()` wrote `groups[]` one element at a time

Two validated `set_array_element` crossings per capture group, per match,
plus a per-element clear loop on the two "no match" paths. All three now go
through one bulk `write_int_array_from`, with the element-at-a-time loop kept
as the fallback for a heap that declines the bulk path (the unit-test mocks
do).

### …and one shared cause: three heap round-trips for one header

`sb_view` asked `object_is_array`, `array_length` and `heap_element_type_of`
about the same payload array. Each repeats the same membership walk and
forwarding barrier — and `is_object_address` alone is 5.4 % of the builder
phase. `NativeHeapAccess::array_shape` answers all three from one validated
header read; its default impl composes the old three, so every mock and
alternate implementation keeps working unchanged.

## Result

Interleaved A/B, one host, one binary pair, arm order flipped on alternate
pairs, 15 pairs, `taskset` to one core, checksum verified on all 30 samples
(`500000500000`, zero mismatches). n = 1 000 000. The `SrPhases` probe times
the two phases IN-PROCESS, which keeps ~1.3 s of fixed VM startup — pure noise
for a delta — out of the numbers.

| phase | base | trial | delta |
|-------|------|-------|-------|
| `StringBuilder` build loop | 2 401 ms | 1 707 ms | **−28.9 %** |
| regex `find`/`group`/`parseLong` loop | 4 698 ms | 4 293 ms | **−8.6 %** |
| kernel (build + regex, per-sample sum) | 6 720 ms | 6 134 ms | **−8.7 %** |

A separate 15-pair run in `mode=all` put the builder phase at 1 815 → 1 174 ms
(**−35.3 %**), with 13 of the 15 trial samples below EVERY base sample — the
one unambiguous separation in the series. A 9-pair end-to-end
`StringRegexOnly` A/B agreed at the kernel level: wall median 5 351 → 4 933 ms
(−7.8 %), CPU-time mean 3 937 → 3 596 ms (−8.7 %).

**Caveat on precision.** The host carried a 1-minute load average of 45–150 on
8 cores throughout, with no idle core available (`mpstat` showed every core
70–95 % busy), and this VM exposes no hardware PMU, so `instructions:u` is
unavailable and `task-clock` is the only load-robust counter. Run-to-run
spread is ±40 %. The builder-phase result is far outside that spread; the
regex-loop and end-to-end numbers are directionally consistent across three
independent series but should be re-taken in a genuine quiet window before
being published.

### What the profile says afterwards

Same `perf record`, builder phase, base vs trial — the four targets are gone
and nothing replaced them:

| symbol | base | trial |
|--------|------|-------|
| `resolve_field_index_by_class_id` | 2.21 % | — |
| `set_field_by_name` | 1.70 % | — |
| `resolve_field_descriptor_byte_cached` | 1.87 % | — |
| `coerce_field_value_for_slot` | 1.02 % | — |
| `load_and_forward_inner` | 1.19 % | — |
| `__memcmp_evex_movbe` | 3.23 % | 1.30 % |
| (kernel, page-fault) | 9.18 % | 5.87 % |

What is left at the top is structural and is the next lever, not this one:
`safe_native_call_impl` (4.1 %), `try_jit_site_cached_native_dispatch`
(3.3 %) and `is_object_address` (4.1 %) — the JIT→native boundary and the
membership walk behind every validated heap access. `StringRegexOnly` crosses
that boundary three times per element of its input (`append` ×2, then
`find`/`group`/`parseLong`), and no amount of work inside the natives touches
it. The residual `__memcmp` and the 1.3 % now attributed to `sb_class_slot`
are the memo's own `&str` name compare, which a field-name enum would remove.

## What was NOT changed

The published `README.md` / `BENCHMARK.md` tables are untouched. Updating a
published row needs a fresh interleaved seven-phase series in a quiet window
under that file's own methodology, not a single-row A/B; this document records
the change and the single-row measurement, and the table should be re-measured
as a whole when a window is available.

## Gates

- `SrParity` (58 lines) and `apps/probes/StringBuilderShadowSweep.java`
  (747 lines) are byte-identical to HotSpot on both arms. `SrParity` was
  written for this change and covers what it touches: multi-group and
  non-participating captures, the failing-`find()` clear path, `find(int)`,
  `region()`, zero-width matches, `appendReplacement`, `matches()`/
  `lookingAt()` populating `groups[]` without the fast path, every integral
  `append` overload including `Integer.MIN_VALUE` / `Long.MIN_VALUE`, an
  append past the 64-unit stack threshold, a non-LATIN1 builder, and
  `StringBuffer`'s `toStringCache` invalidation.
- `StringRegexOnly` checksums match on both arms at n = 100 000 and
  n = 1 000 000.
- `cargo test -p cratonvm-native-builtins --lib`: 4 228 passed, 0 failed.
- `cargo test -p cratonvm-native-api`: all green.
- `cargo clippy -p cratonvm-native-builtins -p cratonvm-native-api`: no new
  warnings.
