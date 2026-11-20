// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! String, StringBuilder, and StringBuffer native method implementations.

use crate::lang_class::ClassTextName;
use cratonvm_native_api::{NativeContext, NativeHandleScope, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult};
use cratonvm_types::Value;

use crate::{compile_java_regex, native_noop_with_this, obj_arg, try_alloc_concurrent_synthetic};
/// Epoch-day/calendar math for `%t`/`%T` formatting, self-contained rather
/// than reusing `util_time`'s equivalents: that module sits behind the
/// `synthetic-jdk` feature, but `String.format`/`Formatter` (this file) is a
/// core native available regardless of feature flags.
use crate::civil_date;
// The JDK-vs-Rust Unicode version skew — `JDK_UNMAPPED_CASE_RUNS` and
// its predicate — used to be declared HERE, and `case_map.rs` had no arm for it
// at all. Those two files are the only case-mapping code in the crate and they
// split the work by locale: this one handles the root and ordinary locales,
// `case_map.rs` handles `tr`/`az`/`lt`. A constant that lives in one half is a
// fix that half the locales never get, which is exactly what happened —
// `"ꟓ".toUpperCase(Locale.ROOT)` was right and
// `"ꟓ".toUpperCase(Locale.forLanguageTag("tr"))` was wrong. The definition now
// lives next to the table it corrects; see
// [`crate::case_map::JDK_UNMAPPED_CASE_RUNS`] for the measurement and
// for why a THIRD copy was not the answer.
use crate::case_map::is_jdk_unmapped_case_code_point;
#[cfg(test)]
mod tests;

/// F22 — the UTF-16-unit formatter pipeline and the `%t` zone.
///
/// Every expectation here was measured on HotSpot 25.0.3+9-LTS before it was
/// written, and each test names the wrong implementation it kills. These are
/// PURE functions asserted directly: routing them through a `NativeContext`
/// mock would have measured the mock's name-to-slot fallback rather than the
/// rule.
#[cfg(test)]
mod f22_utf16_formatter_tests;



/// gc-common w9-d (`common-w8b-native-factories-still-single-attempt`): the
/// caller-sized factories in this file reclaim and retry before their
/// `OutOfMemoryError`. The mock refuses on demand
/// (`test_utils::mock_refuse_allocations`, thread-local) and counts reclaims.
#[cfg(test)]
mod r23_w4_string_regex_scratch_tests;

#[cfg(test)]
mod w9d_reclaiming_factory_tests;

/// gc-common w15-e: `string_array_from_parts` and `int_stream_of` root the
/// object they write into across the allocations that follow it (the mock
/// moves nothing on an allocation, so these pin down the values and the pin
/// balance), and `String.hashCode`'s slot latch is per VM. Each test owns its
/// VM identity (`0xE15_02xx`).
#[cfg(test)]
mod w15e_stale_reference_tests;

/// gc-common w20-b (`common-w19g-init-string-from-units-never-collects`): the
/// in-place String fill goes through the shared GC-and-retry ladder
/// ([`init_string_from_units_or_oom`]). The refusal knob
/// (`test_utils::mock_refuse_allocations`) reaches the mock's
/// `init_string_from_units` only once
/// `handoff-w20b-mock-init-string-honours-refusals` is applied; until then
/// the refusal tests detect that and skip.
#[cfg(test)]
mod w20b_init_string_retry_tests;

/// JIT round 17 wave 2, lane streams2: `String.lines()` cut points
/// (`CRATONVM_STRING_LINES_UNITS_STREAM`). Expectations are JDK 25
/// `LinesSpliterator`'s.
#[cfg(test)]
mod r17_streams2_lines_tests;

/// gci i4/wr (`gcg-g2w-string-table-review-findings` item 2): the surrogate
/// pool's replace-if-dead claim, and its use by `String.intern()`'s
/// lone-surrogate path under a weak pool.
#[cfg(test)]
mod gci_i4wr_surrogate_pool_tests;

/// JIT round 23 wave 2, lane L6 (`CRATONVM_STRING_ERRORS_FROM_JDK_BODY`): the
/// code-point natives' failing paths.
#[cfg(test)]
mod r23_w2_l6_string_errors_from_jdk_body_tests;

#[cfg(test)]
mod avx512_u16_search_tests;

mod decode_string_chars;
#[allow(unused_imports)]
pub use decode_string_chars::*;
#[allow(unused_imports)]
use decode_string_chars::*;
mod native_sb_get_chars;
#[allow(unused_imports)]
pub use native_sb_get_chars::*;
#[allow(unused_imports)]
use native_sb_get_chars::*;
mod exact_regex_replace_units;
#[allow(unused_imports)]
pub use exact_regex_replace_units::*;
#[allow(unused_imports)]
use exact_regex_replace_units::*;
mod fmt_temporal_fault_char;
#[allow(unused_imports)]
pub use fmt_temporal_fault_char::*;
#[allow(unused_imports)]
use fmt_temporal_fault_char::*;
mod bounds_off_count_violation;
#[allow(unused_imports)]
pub use bounds_off_count_violation::*;
#[allow(unused_imports)]
use bounds_off_count_violation::*;
