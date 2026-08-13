// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! REGISTRAR REACHABILITY GATE — a registrar may not become synthetic-only
//! without being listed here as deliberate.
//!
//! # The species
//!
//! `register_builtins` -> `register_synthetic_overrides` is
//! `#[cfg(feature = "synthetic-jdk")]`, and `synthetic-jdk` is in no crate's
//! default feature set. A registration pass whose only call-site chain runs
//! through `register_synthetic_overrides` is therefore **not in the shipping
//! `cratonvm-cli` binary at all** — not "present and declined by policy" in
//! `--jdk-only`, not there to decline. Its `register(...)` calls read as
//! coverage and are not coverage, and every test that exercises it is
//! measuring an implementation the shipping modes never run.
//!
//! That has misled this campaign repeatedly. The canonical instance is
//! `panama.rs::register_pe_panama`: its sole call site is inside
//! `register_synthetic_overrides`, so the two shipping modes ran a *different*
//! `structLayout` implementation (`phases_late/foreign_ffm.rs`) from the one
//! every test covered, and the one that ships was wrong six ways.
//! `phases_late::register_p64_hex_format` was the same shape and has since been
//! promoted — `lib.rs::register_hex_format_real_jdk_natives` now calls it from
//! `register_essential_natives_with_shims`, deliberately LAST because
//! `register()` is last-write-wins. Both are pinned as controls in
//! [`the_scanner_is_not_vacuous`], one positive and one negative, so this file
//! cannot pass by finding nothing.
//!
//! # What this gate is, and what already existed
//!
//! The *census* is not new. `docs/known-issues/jdk-only/W7-5-registrars-that-
//! never-shipped.md` (2026-08-12) counted 301 registrars absent from the
//! default build, 278 of them reachable only via `register_synthetic_overrides`,
//! across five crates. What did not exist is the thing a document cannot do:
//! **fail**. W7-5 §6.3 asked for a ratchet and what landed
//! (`essential_wiring_ratchet.rs`) ratchets six named *triples* — it says
//! nothing about the population, and cannot notice a 74th family appearing or a
//! shipping registrar losing its last shipping call site.
//!
//! This file is that population ratchet, scoped to `native-builtins` (the crate
//! that owns `register_synthetic_overrides`). Two assertions with different
//! jobs:
//!
//! 1. [`no_new_synthetic_only_family`] — the synthetic-only DIRECT children of
//!    `register_synthetic_overrides` must be exactly
//!    [`DELIBERATE_SYNTHETIC_ONLY_FAMILIES`], each carrying a reason. A new
//!    family, or a family promoted to the shipping path, fails.
//! 2. [`no_registrar_silently_orphaned_into_the_synthetic_arm`] — the full
//!    transitive synthetic-only set must be exactly [`SYNTHETIC_ONLY_CLOSURE`].
//!    This is the one that catches the `register_p64_hex_format` shape *in
//!    reverse*: a pass that today has a shipping call site and tomorrow loses
//!    it enters the closure, and the set no longer matches. Assertion 1 alone
//!    would miss that, because such a pass is usually buried inside an
//!    already-allow-listed family's subtree.
//!
//! # Why a SOURCE witness and not a registry dump
//!
//! The two boot paths live behind different feature configurations, so a
//! `#[cfg(feature = ...)]` test only ever guards the configuration it is
//! compiled into — and the whole defect is that one configuration is invisible
//! from the other. A source scan guards both, and it runs with no VM boot.
//!
//! It reads the working tree through [`env!("CARGO_MANIFEST_DIR")`], **not**
//! `include_str!`. `include_str!` bakes a snapshot into the test binary at
//! compile time, which is the same "frozen model of a file that keeps moving"
//! failure this gate exists to prevent.
//!
//! # Keyed on the ARGUMENT, not the name
//!
//! A registration pass need not be called `register_*`. F30's first scanner
//! keyed on the name prefix and missed `vm_init.rs::init_service_loader_bootstrap`
//! for exactly that reason. [`scan`] keys on a signature mentioning
//! `NativeMethodRegistry`, so a pass named anything at all is in the
//! population.
//!
//! # Non-vacuity
//!
//! A source witness that finds nothing passes loudly, so every stage is
//! floored: file count, `fn` count, pass count, direct-child count, shipping
//! set size, external-reference count, and the byte size of the extracted
//! `register_synthetic_overrides` body. Plus the two-sided control pair above.
//! See [`the_scanner_is_not_vacuous`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// ===========================================================================
// SECTION 1 — the allow-lists
// ===========================================================================

/// Direct children of `register_synthetic_overrides` that are deliberately
/// synthetic-only, each with the measured reason it is not a `--jdk-only`
/// capability gap.
///
/// The reasons are MEASURED, not asserted from names. For each family the
/// census took (a) the set of Java classes it registers, transitively; (b) how
/// many of those classes no shipping-side registrar in `native-builtins`,
/// `native-io`, `native-collections`, `native-builtins-crypto`,
/// `native-builtins-security` or `vm` touches ("exclusive"); (c) whether each
/// exclusive class exists in JDK 25.0.3+9-LTS and whether it declares any
/// `native` method (`Class.forName` + `getDeclaredMethods`, boot then platform
/// then system loader); and (d) how many `(class, name, descriptor)` triples
/// the family registers that a shipping pass also registers ("drift").
///
/// The four verdicts:
///
/// * **SHIPPING TWIN** — nothing it registers is exclusive to it. Real-JDK mode
///   gets the same classes from a shipping registrar. The two implementations
///   can still drift, and the trailing `N/M triples` is the size of that
///   exposure; that is a separate problem from reachability and is recorded in
///   this lane's F34-1 record, not gated here.
/// * **REAL-JDK BYTECODE** — it has exclusive classes, all of which exist in
///   JDK 25 and declare **no** `native` method, so real JDK bytecode serves
///   them in `--jdk-only`. Wiring the registrar in would REPLACE working JDK
///   code with a partial Rust reimplementation (W7-5 §3.1).
/// * **APP/ABSENT** — some exclusive classes do not exist in JDK 25 at all
///   (third-party: slf4j, log4j, jackson, gson, GraalVM, Spring; or renamed:
///   `jdk/incubator/concurrent/StructuredTaskScope`, `java/net/PlainSocketImpl`).
///   Nothing in `--jdk-only` can reference them, so the absence is inert.
/// * **TOMBSTONE** — registers nothing at all. Several are deliberately empty
///   functions whose bodies are a comment explaining why they must stay empty
///   (`t3_impl.rs::register_t31_structured_concurrency` is one: registering
///   competing `StructuredTaskScope` stubs overrode the canonical 8-field
///   layout with an earlier 2-field one).
///
/// **A verdict here is a claim about REACHABILITY, not about correctness.** It
/// says the shipping mode is not missing a capability because of this
/// registrar's gating. It does not say the registrar is right.
///
/// To add a row you must state which of the four it is and why. To delete a
/// row, wire the family onto the shipping path — and read the
/// `register_p64_hex_format` promotion first, because `register()` is
/// last-write-wins and call ORDER decides which implementation survives.
const DELIBERATE_SYNTHETIC_ONLY_FAMILIES: &[(&str, &str)] = &[
    ("register_aot_natives",
     "REAL-JDK BYTECODE: all 1 exclusive classes exist in JDK 25 and declare no native method; 1/14 triples also on the shipping path"),
    ("register_atomic_boolean_natives",
     "SHIPPING TWIN: no class exclusive to it; 8/8 triples also registered by a shipping pass"),
    ("register_bigdecimal_natives",
     "SHIPPING TWIN: no class exclusive to it; 18/32 triples also registered by a shipping pass"),
    ("register_biginteger_natives",
     "SHIPPING TWIN: no class exclusive to it; 19/32 triples also registered by a shipping pass"),
    ("register_byte_array_output_stream",
     "SHIPPING TWIN: no class exclusive to it; 0/12 triples also registered by a shipping pass"),
    ("register_cds_natives",
     "REAL-JDK BYTECODE: all 1 exclusive classes exist in JDK 25 and declare no native method; 0/11 triples also on the shipping path"),
    ("register_classfile_api_natives",
     "REAL-JDK BYTECODE: all 16 exclusive classes exist in JDK 25 and declare no native method; 0/78 triples also on the shipping path"),
    ("register_classloader_natives",
     "SHIPPING TWIN: no class exclusive to it; 26/52 triples also registered by a shipping pass"),
    ("register_completable_future_natives",
     "REAL-JDK BYTECODE: all 1 exclusive classes exist in JDK 25 and declare no native method; 0/23 triples also on the shipping path"),
    ("register_concurrent_extras",
     "SHIPPING TWIN: no class exclusive to it; 2/15 triples also registered by a shipping pass"),
    ("register_crypto_impl_natives",
     "SHIPPING TWIN: no class exclusive to it; 2/2 triples also registered by a shipping pass"),
    ("register_enterprise_final_natives",
     "APP/ABSENT: 1 of its 3 exclusive classes do not exist in JDK 25, so --jdk-only bytecode cannot reference them; 122/232 triples also on the shipping path"),
    ("register_enterprise_natives",
     "SHIPPING TWIN: no class exclusive to it; 0/8 triples also registered by a shipping pass"),
    ("register_enum_natives",
     "SHIPPING TWIN: no class exclusive to it; 4/8 triples also registered by a shipping pass"),
    ("register_functional_completion_natives",
     "SHIPPING TWIN: no class exclusive to it; 0/8 triples also registered by a shipping pass"),
    ("register_functional_extras_natives",
     "TOMBSTONE: registers nothing (deliberately empty or dynamic-only); no capability rides on it"),
    ("register_graalvm_compat_natives",
     "APP/ABSENT: 8 of its 8 exclusive classes do not exist in JDK 25, so --jdk-only bytecode cannot reference them; 0/15 triples also on the shipping path"),
    ("register_http2_natives",
     "APP/ABSENT: 1 of its 2 exclusive classes do not exist in JDK 25, so --jdk-only bytecode cannot reference them; 38/90 triples also on the shipping path"),
    ("register_jackson_gson_natives",
     "APP/ABSENT: 2 of its 2 exclusive classes do not exist in JDK 25, so --jdk-only bytecode cannot reference them; 0/38 triples also on the shipping path"),
    ("register_java_lang_extras_natives",
     "SHIPPING TWIN: no class exclusive to it; 3/14 triples also registered by a shipping pass"),
    ("register_jdk25_concurrency_natives",
     "SHIPPING TWIN: no class exclusive to it; 0/64 triples also registered by a shipping pass"),
    ("register_jdk25_language_natives",
     "SHIPPING TWIN: no class exclusive to it; 0/9 triples also registered by a shipping pass"),
    ("register_jdk25_patterns_natives",
     "SHIPPING TWIN: no class exclusive to it; 0/15 triples also registered by a shipping pass"),
    ("register_letsgo_compat_natives",
     "SHIPPING TWIN: no class exclusive to it; 0/4 triples also registered by a shipping pass"),
    ("register_locale_natives",
     "TOMBSTONE: registers nothing (deliberately empty or dynamic-only); no capability rides on it"),
    ("register_logging_natives",
     "SHIPPING TWIN: no class exclusive to it; 20/30 triples also registered by a shipping pass"),
    ("register_m18_concurrent_fixes",
     "SHIPPING TWIN: no class exclusive to it; 2/41 triples also registered by a shipping pass"),
    ("register_number_format_natives",
     "TOMBSTONE: registers nothing (deliberately empty or dynamic-only); no capability rides on it"),
    ("register_pe_panama",
     "APP/ABSENT: 1 of its 2 exclusive classes do not exist in JDK 25, so --jdk-only bytecode cannot reference them; 38/52 triples also on the shipping path"),
    ("register_phase50_natives",
     "SHIPPING TWIN: no class exclusive to it; 44/129 triples also registered by a shipping pass"),
    ("register_phase51_natives",
     "SHIPPING TWIN: no class exclusive to it; 76/173 triples also registered by a shipping pass"),
    ("register_phase52_natives",
     "REAL-JDK BYTECODE: all 7 exclusive classes exist in JDK 25 and declare no native method; 61/181 triples also on the shipping path"),
    ("register_phase53_natives",
     "SHIPPING TWIN: no class exclusive to it; 112/152 triples also registered by a shipping pass"),
    ("register_phase54_natives",
     "SHIPPING TWIN: no class exclusive to it; 149/200 triples also registered by a shipping pass"),
    ("register_phase55_natives",
     "REAL-JDK BYTECODE: all 1 exclusive classes exist in JDK 25 and declare no native method; 32/90 triples also on the shipping path"),
    ("register_phase56_natives",
     "SHIPPING TWIN: no class exclusive to it; 42/139 triples also registered by a shipping pass"),
    ("register_phase57_natives",
     "SHIPPING TWIN: no class exclusive to it; 241/306 triples also registered by a shipping pass"),
    ("register_phase58_natives",
     "APP/ABSENT: 1 of its 3 exclusive classes do not exist in JDK 25, so --jdk-only bytecode cannot reference them; 25/170 triples also on the shipping path"),
    ("register_phase59_natives",
     "SHIPPING TWIN: no class exclusive to it; 149/206 triples also registered by a shipping pass"),
    ("register_phase60_natives",
     "SHIPPING TWIN: no class exclusive to it; 46/79 triples also registered by a shipping pass"),
    ("register_phase61_natives",
     "APP/ABSENT: 1 of its 2 exclusive classes do not exist in JDK 25, so --jdk-only bytecode cannot reference them; 45/121 triples also on the shipping path"),
    ("register_phase62_natives",
     "REAL-JDK BYTECODE: all 1 exclusive classes exist in JDK 25 and declare no native method; 29/109 triples also on the shipping path"),
    ("register_phase63_natives",
     "REAL-JDK BYTECODE: all 1 exclusive classes exist in JDK 25 and declare no native method; 47/68 triples also on the shipping path"),
    ("register_phase64_natives",
     "REAL-JDK BYTECODE: all 2 exclusive classes exist in JDK 25 and declare no native method; 26/82 triples also on the shipping path"),
    ("register_phase65_natives",
     "REAL-JDK BYTECODE: all 1 exclusive classes exist in JDK 25 and declare no native method; 15/81 triples also on the shipping path"),
    ("register_phase66_natives",
     "REAL-JDK BYTECODE: all 1 exclusive classes exist in JDK 25 and declare no native method; 25/71 triples also on the shipping path"),
    ("register_phase67_natives",
     "APP/ABSENT: 3 of its 6 exclusive classes do not exist in JDK 25, so --jdk-only bytecode cannot reference them; 158/219 triples also on the shipping path"),
    ("register_phase68_natives",
     "REAL-JDK BYTECODE: all 14 exclusive classes exist in JDK 25 and declare no native method; 290/372 triples also on the shipping path"),
    ("register_phase69_natives",
     "APP/ABSENT: 1 of its 4 exclusive classes do not exist in JDK 25, so --jdk-only bytecode cannot reference them; 5/70 triples also on the shipping path"),
    ("register_phase70_natives",
     "SHIPPING TWIN: no class exclusive to it; 22/77 triples also registered by a shipping pass"),
    ("register_phase71_natives",
     "REAL-JDK BYTECODE: all 2 exclusive classes exist in JDK 25 and declare no native method; 74/171 triples also on the shipping path"),
    ("register_phase72_natives",
     "APP/ABSENT: 1 of its 7 exclusive classes do not exist in JDK 25, so --jdk-only bytecode cannot reference them; 92/155 triples also on the shipping path"),
    ("register_phase_d_natives",
     "REAL-JDK BYTECODE: all 1 exclusive classes exist in JDK 25 and declare no native method; 0/24 triples also on the shipping path"),
    ("register_quarkus_arc_natives",
     "SHIPPING TWIN: no class exclusive to it; 0/7 triples also registered by a shipping pass"),
    ("register_s1_classloading",
     "SHIPPING TWIN: no class exclusive to it; 17/22 triples also registered by a shipping pass"),
    ("register_s2_nio",
     "SHIPPING TWIN: no class exclusive to it; 130/164 triples also registered by a shipping pass"),
    ("register_s3_http_client",
     "SHIPPING TWIN: no class exclusive to it; 4/4 triples also registered by a shipping pass"),
    ("register_security_natives",
     "REAL-JDK BYTECODE: all 1 exclusive classes exist in JDK 25 and declare no native method; 38/41 triples also on the shipping path"),
    ("register_serialization_natives",
     "REAL-JDK BYTECODE: all 10 exclusive classes exist in JDK 25 and declare no native method; 2/95 triples also on the shipping path"),
    ("register_slf4j_natives",
     "APP/ABSENT: 4 of its 4 exclusive classes do not exist in JDK 25, so --jdk-only bytecode cannot reference them; 48/102 triples also on the shipping path"),
    ("register_t25_natives",
     "REAL-JDK BYTECODE: all 1 exclusive classes exist in JDK 25 and declare no native method; 1/29 triples also on the shipping path"),
    ("register_t310_scripting",
     "REAL-JDK BYTECODE: all 2 exclusive classes exist in JDK 25 and declare no native method; 2/9 triples also on the shipping path"),
    ("register_t311_i18n",
     "SHIPPING TWIN: no class exclusive to it; 1/2 triples also registered by a shipping pass"),
    ("register_t312_tooling",
     "REAL-JDK BYTECODE: all 10 exclusive classes exist in JDK 25 and declare no native method; 0/15 triples also on the shipping path"),
    ("register_t31_concurrent_extras",
     "REAL-JDK BYTECODE: all 2 exclusive classes exist in JDK 25 and declare no native method; 2/28 triples also on the shipping path"),
    ("register_t31_structured_concurrency",
     "TOMBSTONE: registers nothing (deliberately empty or dynamic-only); no capability rides on it"),
    ("register_t38_jndi",
     "REAL-JDK BYTECODE: all 2 exclusive classes exist in JDK 25 and declare no native method; 7/16 triples also on the shipping path"),
    ("register_t39_stax",
     "REAL-JDK BYTECODE: all 6 exclusive classes exist in JDK 25 and declare no native method; 11/26 triples also on the shipping path"),
    ("register_time_extras_natives",
     "REAL-JDK BYTECODE: all 1 exclusive classes exist in JDK 25 and declare no native method; 1/115 triples also on the shipping path"),
    ("register_time_natives",
     "SHIPPING TWIN: no class exclusive to it; 16/89 triples also registered by a shipping pass"),
    ("register_tls_natives",
     "REAL-JDK BYTECODE: all 2 exclusive classes exist in JDK 25 and declare no native method; 53/113 triples also on the shipping path"),
    ("register_unsafe_define_class",
     "SHIPPING TWIN: no class exclusive to it; 12/13 triples also registered by a shipping pass"),
    ("register_vector_api_natives",
     "SHIPPING TWIN: no class exclusive to it; 0/136 triples also registered by a shipping pass"),
];

/// The full transitive synthetic-only set: every pass in `native-builtins`
/// reachable from `register_synthetic_overrides` and from nowhere on the
/// shipping side.
///
/// Machine-derived and deliberately carries no prose — the prose lives on the
/// families above, and a 284-line list of hand-written justifications would be
/// fiction. Its job is to be an exact set: any name entering or leaving fails
/// [`no_registrar_silently_orphaned_into_the_synthetic_arm`] with a diff.
///
/// This is a RATCHET IN BOTH DIRECTIONS. A name leaving the list is good news
/// (something was promoted to the shipping path) and still fails, because the
/// list must record it. Shrink it in the same commit as the promotion.
const SYNTHETIC_ONLY_CLOSURE: &[&str] = &[
    "register_aot_natives",
    "register_arc_container",
    "register_arc_facade",
    "register_atomic_boolean_natives",
    "register_attribute",
    "register_bean_manager",
    "register_bigdecimal_natives",
    "register_biginteger_natives",
    "register_bitset_natives",
    "register_blocking_queue_drain_to",
    "register_body_handlers",
    "register_body_publisher",
    "register_byte_array_output_stream",
    "register_byte_vector",
    "register_calendar_natives",
    "register_cds_natives",
    "register_class_builder",
    "register_class_model",
    "register_class_transform",
    "register_classfile",
    "register_classfile_api_natives",
    "register_classloader_define_class",
    "register_classloader_natives",
    "register_code_builder",
    "register_code_model",
    "register_code_transform",
    "register_completable_future_natives",
    "register_concurrent_extras",
    "register_constant_pool",
    "register_core_stdlib_extras",
    "register_crypto_impl_natives",
    "register_currency_natives",
    "register_double_vector",
    "register_enterprise_final_natives",
    "register_enterprise_natives",
    "register_entry_value_semantics",
    "register_enum_map_natives",
    "register_enum_natives",
    "register_enum_set_natives",
    "register_es4_elasticsearch_stubs",
    "register_exception_type",
    "register_exchanger_natives",
    "register_field_model",
    "register_float_vector",
    "register_forkjoin_extras",
    "register_forkjoin_natives",
    "register_formatter_natives",
    "register_functional_completion_natives",
    "register_functional_extras_natives",
    "register_graalvm_compat_natives",
    "register_http2_natives",
    "register_http_client",
    "register_http_client_builder",
    "register_http_headers",
    "register_http_request",
    "register_http_request_builder",
    "register_http_response",
    "register_identity_hashmap_natives",
    "register_injectable_bean",
    "register_instance_handle",
    "register_int_vector",
    "register_invalid_class_exception",
    "register_jackson_gson_natives",
    "register_java_lang_extras_natives",
    "register_jdk25_concurrency_natives",
    "register_jdk25_language_natives",
    "register_jdk25_patterns_natives",
    "register_key_manager_factory",
    "register_key_store",
    "register_keycloak_tls_natives",
    "register_letsgo_compat_natives",
    "register_locale_natives",
    "register_logging_natives",
    "register_long_vector",
    "register_m18_concurrent_fixes",
    "register_method_model",
    "register_not_serializable_exception",
    "register_number_format_natives",
    "register_object_input",
    "register_object_input_filter",
    "register_object_input_stream",
    "register_object_output",
    "register_object_output_stream",
    "register_object_stream_class",
    "register_object_stream_field",
    "register_object_stream_natives",
    "register_p58_completable_future",
    "register_p58_gzip_streams",
    "register_p58_nio_channels",
    "register_p58_pushback",
    "register_p58_string_concat_factory",
    "register_p58_synchronous_queue",
    "register_p59_management",
    "register_p59_module",
    "register_p59_package",
    "register_p59_spliterator",
    "register_p59_varhandle",
    "register_p60_abstract_map",
    "register_p60_callsite",
    "register_p60_flow",
    "register_p60_http_client",
    "register_p60_match_result",
    "register_p60_record",
    "register_p61_charset",
    "register_p61_classloader",
    "register_p61_files_path",
    "register_p61_handler_error_manager",
    "register_p61_logging",
    "register_p61_net",
    "register_p61_reflect",
    "register_p61_text_formatting",
    "register_p62_abstract_map_entries",
    "register_p62_format_factories",
    "register_p62_navigable_expansion",
    "register_p62_time_expansion",
    "register_p62_zip_entry",
    "register_p63_enumeration",
    "register_p63_formatter",
    "register_p63_resource_bundle",
    "register_p63_scheduled_executor",
    "register_p63_service_loader",
    "register_p63_weak_hash_map",
    "register_p64_collectors_teeing",
    "register_p64_math_clamp",
    "register_p64_random_generator",
    "register_p64_sequenced_collections",
    "register_p64_stream_modern",
    "register_p64_string_additions",
    "register_p65_checked_collections",
    "register_p65_completion_service",
    "register_p65_datetime_builder",
    "register_p65_delay_queue",
    "register_p65_method_handles_extra",
    "register_p65_pattern_additions",
    "register_p65_priority_blocking_queue",
    "register_p65_stream_map_multi",
    "register_p66_collator",
    "register_p66_constant_desc",
    "register_p66_pushback_reader",
    "register_p66_thread_builder",
    "register_p66_watch_service",
    "register_p67_gatherer",
    "register_p67_misc",
    "register_p67_scoped_value",
    "register_p67_structured_task_scope",
    "register_p67_structured_task_scope_j25",
    "register_p68_jdbc_driver_manager",
    "register_p68_xml",
    "register_p69_compact_number_format",
    "register_p69_misc",
    "register_p69_spliterator",
    "register_p69_submission_publisher",
    "register_p69_switch_bootstraps",
    "register_p69_websocket",
    "register_p70_atomic_accumulators",
    "register_p70_constant_bootstraps",
    "register_p70_misc",
    "register_p70_object_streams",
    "register_p71_deflater_inflater",
    "register_p71_files_bridge",
    "register_p71_logging_extras",
    "register_p71_wrapper_extras",
    "register_p71_zip_extras",
    "register_p72_datagram",
    "register_p72_http_server",
    "register_p72_naming",
    "register_p72_preferences",
    "register_p72_server_socket",
    "register_pbe_diagnostic",
    "register_pd_scoped_values",
    "register_pd_stream_gatherers",
    "register_pd_structured_concurrency",
    "register_pe2_string_marshaling",
    "register_pe2_struct_layouts",
    "register_pe_arena",
    "register_pe_function_descriptor",
    "register_pe_linker",
    "register_pe_panama",
    "register_pe_value_layout",
    "register_phase50_natives",
    "register_phase51_natives",
    "register_phase52_byte_order",
    "register_phase52_chrono_unit",
    "register_phase52_clock",
    "register_phase52_date_format",
    "register_phase52_function_extras",
    "register_phase52_math_context",
    "register_phase52_message_format",
    "register_phase52_natives",
    "register_phase52_objects_extras",
    "register_phase52_offset_datetime",
    "register_phase52_rounding_mode",
    "register_phase52_string_buffer",
    "register_phase52_time_enums",
    "register_phase52_url_encoding",
    "register_phase53_crypto",
    "register_phase53_natives",
    "register_phase53_record",
    "register_phase53_sealed",
    "register_phase53_security",
    "register_phase53_service_loader",
    "register_phase53_socket_stubs",
    "register_phase54_atomics",
    "register_phase54_natives",
    "register_phase54_net_extras",
    "register_phase54_zip_stubs",
    "register_phase55_charset",
    "register_phase55_collection_extras",
    "register_phase55_executors",
    "register_phase55_natives",
    "register_phase55_reflect",
    "register_phase56_collectors_extras",
    "register_phase56_natives",
    "register_phase56_stream_extras",
    "register_phase56_summary_stats",
    "register_phase57_file_channel",
    "register_phase57_natives",
    "register_phase57_random_access_file",
    "register_phase57_text",
    "register_phase58_natives",
    "register_phase59_natives",
    "register_phase60_natives",
    "register_phase61_natives",
    "register_phase62_natives",
    "register_phase63_natives",
    "register_phase64_natives",
    "register_phase65_natives",
    "register_phase66_natives",
    "register_phase67_natives",
    "register_phase68_natives",
    "register_phase69_natives",
    "register_phase70_natives",
    "register_phase71_natives",
    "register_phase72_natives",
    "register_phase_d_natives",
    "register_phaser_natives",
    "register_quarkus_arc_natives",
    "register_quarkus_arc_natives_unconditional",
    "register_reflection_factory_serialization",
    "register_s1_classloading",
    "register_s2_nio",
    "register_s2_selector",
    "register_s2_server_socket_channel",
    "register_s2_socket_channel",
    "register_s3_http_client",
    "register_security_natives",
    "register_serializable",
    "register_serialization_natives",
    "register_short_vector",
    "register_slf4j_natives",
    "register_ssl_context",
    "register_ssl_context_impl",
    "register_ssl_engine",
    "register_ssl_engine_result",
    "register_ssl_parameters",
    "register_ssl_session",
    "register_ssl_socket_factory",
    "register_stream_corrupted_exception",
    "register_string_tokenizer_natives",
    "register_synchronous_queue_extras",
    "register_t25_natives",
    "register_t310_scripting",
    "register_t311_i18n",
    "register_t312_tooling",
    "register_t31_concurrent_extras",
    "register_t31_structured_concurrency",
    "register_t38_jndi",
    "register_t39_stax",
    "register_time_extras_natives",
    "register_time_natives",
    "register_timer_natives",
    "register_timeunit_natives",
    "register_timezone_natives",
    "register_tls_natives",
    "register_trust_manager_factory",
    "register_unsafe_define_class",
    "register_vector_api_natives",
    "register_vector_mask",
    "register_vector_operators",
    "register_vector_shuffle",
    "register_vector_species",
    "register_weak_hashmap_natives",
    "register_websocket",
    "register_x509_trust_manager",
];

/// The pass every synthetic-only chain runs through.
const SYNTHETIC_OVERRIDES: &str = "register_synthetic_overrides";

// --- non-vacuity floors ----------------------------------------------------
// Every one of these is a MEASURED value at 2026-08-13 minus generous slack,
// so ordinary growth does not redden them but a scanner that silently stops
// finding things does. A floor with no measurement behind it is decoration.

/// `fn` definitions parsed out of `native-builtins/src` (measured 18,516).
const MIN_FN_DEFS: usize = 12_000;
/// Definitions whose signature mentions `NativeMethodRegistry` (measured 754).
const MIN_PASSES: usize = 550;
/// Distinct passes called directly by `register_synthetic_overrides` (117).
const MIN_DIRECT_CHILDREN: usize = 90;
/// Passes reachable from the shipping side (measured 407).
const MIN_SHIPPING: usize = 300;
/// Pass names referenced outside `native-builtins/src` (measured 61).
const MIN_EXTERNAL_REFS: usize = 40;
/// Bytes of `register_synthetic_overrides`' extracted body (measured 104,359).
const MIN_SYNTHETIC_OVERRIDES_BODY: usize = 60_000;
/// `.rs` files found outside `native-builtins/src` (measured 826, minus the
/// `tests`/`benches`/`examples` trees this scan skips).
const MIN_WORKSPACE_FILES: usize = 300;
/// `.rs` files found inside `native-builtins/src` (measured 171).
const MIN_CRATE_FILES: usize = 120;

/// The two-sided control pair, from the record that motivated this file.
///
/// `register_pe_panama` MUST be synthetic-only (the live defect) and
/// `register_p64_hex_format` MUST NOT be (the remediated twin). If the scanner
/// ever answers the same way for both, it has stopped discriminating and every
/// other assertion in this file is worthless.
const CONTROL_POSITIVE: &str = "register_pe_panama";
const CONTROL_NEGATIVE: &str = "register_p64_hex_format";

// ===========================================================================
// SECTION 2 — the scanner
// ===========================================================================

mod scan {
    use super::*;

    /// One `.rs` file, with comments and string/char literal *contents* blanked
    /// to spaces. Newlines survive so byte offsets still map to line numbers.
    pub struct FileSrc {
        pub rel: String,
        pub blanked: Vec<u8>,
    }

    /// One `fn` definition.
    pub struct FnDef {
        pub file: usize,
        pub name: String,
        pub line: usize,
        pub indent: usize,
        /// Signature text between the name and the body's `{`.
        pub takes_registry: bool,
        /// Carries `#[cfg(feature = "synthetic-jdk")]`.
        pub syn_gated: bool,
        /// Under `#[cfg(test)]` / `#[test]`, by attribute or by containment in
        /// a `#[cfg(test)] mod` body.
        pub testish: bool,
        /// Byte offsets of the body, `{` .. `}` inclusive.
        pub start: usize,
        pub end: usize,
    }

    #[inline]
    fn is_ident_start(b: u8) -> bool {
        b.is_ascii_alphabetic() || b == b'_'
    }
    #[inline]
    fn is_ident(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }

    /// Replace comment and literal *content* bytes with spaces, preserving
    /// length and newlines.
    ///
    /// Region boundaries are always ASCII, and whole regions are blanked, so a
    /// multi-byte character inside a comment becomes N spaces rather than a
    /// broken code unit — the result is still valid UTF-8. The crate's comments
    /// do contain box-drawing characters, so this matters.
    pub fn blank(src: &[u8]) -> Vec<u8> {
        let mut out = src.to_vec();
        let n = src.len();
        let mut i = 0usize;
        let mut wipe = |out: &mut Vec<u8>, from: usize, to: usize| {
            for b in out[from..to.min(n)].iter_mut() {
                if *b != b'\n' {
                    *b = b' ';
                }
            }
        };
        while i < n {
            let c = src[i];
            if c == b'/' && i + 1 < n && src[i + 1] == b'/' {
                let j = src[i..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(n, |p| i + p);
                wipe(&mut out, i, j);
                i = j;
            } else if c == b'/' && i + 1 < n && src[i + 1] == b'*' {
                // Rust block comments nest.
                let mut depth = 0usize;
                let mut j = i;
                while j < n {
                    if src[j] == b'/' && j + 1 < n && src[j + 1] == b'*' {
                        depth += 1;
                        j += 2;
                    } else if src[j] == b'*' && j + 1 < n && src[j + 1] == b'/' {
                        depth -= 1;
                        j += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        j += 1;
                    }
                }
                wipe(&mut out, i, j);
                i = j;
            } else if c == b'r'
                && (i == 0 || !is_ident(src[i - 1]))
                && i + 1 < n
                && (src[i + 1] == b'"' || src[i + 1] == b'#')
            {
                let mut h = i + 1;
                while h < n && src[h] == b'#' {
                    h += 1;
                }
                if h < n && src[h] == b'"' {
                    let hashes = h - i - 1;
                    let mut j = h + 1;
                    let mut end = n;
                    while j < n {
                        if src[j] == b'"' && src[j + 1..].iter().take(hashes).all(|&b| b == b'#') {
                            end = j + 1 + hashes;
                            break;
                        }
                        j += 1;
                    }
                    wipe(&mut out, i, end);
                    i = end;
                } else {
                    i += 1;
                }
            } else if c == b'"' {
                let mut j = i + 1;
                while j < n {
                    if src[j] == b'\\' {
                        j += 2;
                        continue;
                    }
                    if src[j] == b'"' {
                        j += 1;
                        break;
                    }
                    j += 1;
                }
                wipe(&mut out, i, j);
                i = j;
            } else if c == b'\'' {
                // A char literal, or a lifetime (`'a`, `'static`) which must
                // NOT be treated as an unterminated literal.
                if i + 2 < n && src[i + 1] == b'\\' {
                    if let Some(p) = src[i + 2..].iter().take(4).position(|&b| b == b'\'') {
                        let end = i + 3 + p;
                        wipe(&mut out, i, end);
                        i = end;
                        continue;
                    }
                } else if i + 2 < n && src[i + 2] == b'\'' {
                    wipe(&mut out, i, i + 3);
                    i += 3;
                    continue;
                }
                i += 1;
            } else {
                i += 1;
            }
        }
        out
    }

    /// Spans of `#[cfg(test)] mod ... { .. }` bodies, as byte ranges.
    pub fn cfg_test_spans(b: &[u8]) -> Vec<(usize, usize)> {
        let needle = b"#[cfg(test)]";
        let mut out = Vec::new();
        let mut i = 0usize;
        while i + needle.len() <= b.len() {
            if &b[i..i + needle.len()] == needle {
                // Require `mod` before the next `{`, so `#[cfg(test)]` on a
                // plain `fn` or `use` does not swallow the rest of the file.
                let mut j = i + needle.len();
                let mut saw_mod = false;
                while j < b.len() && b[j] != b'{' {
                    if b[j..].starts_with(b"mod") && (j + 3 >= b.len() || !is_ident(b[j + 3])) {
                        saw_mod = true;
                    }
                    j += 1;
                }
                if saw_mod && j < b.len() {
                    if let Some(end) = match_brace(b, j) {
                        out.push((j, end));
                        i = end;
                        continue;
                    }
                }
            }
            i += 1;
        }
        out
    }

    /// Index of the `}` closing the `{` at `open`.
    fn match_brace(b: &[u8], open: usize) -> Option<usize> {
        let mut d = 0i32;
        let mut k = open;
        while k < b.len() {
            match b[k] {
                b'{' => d += 1,
                b'}' => {
                    d -= 1;
                    if d == 0 {
                        return Some(k);
                    }
                }
                _ => {}
            }
            k += 1;
        }
        None
    }

    /// Parse `fn` headers at the start of any line: `pub(crate) const unsafe fn
    /// name`, `extern "C" fn name`, and so on. Returns the byte offset just
    /// past the name.
    fn parse_fn_header(b: &[u8], mut p: usize) -> Option<(String, usize)> {
        let eat = |b: &[u8], p: &mut usize, kw: &[u8]| -> bool {
            if b[*p..].starts_with(kw) {
                let after = *p + kw.len();
                if after < b.len() && (b[after] == b' ' || b[after] == b'(') {
                    *p = after;
                    while *p < b.len() && b[*p] == b' ' {
                        *p += 1;
                    }
                    return true;
                }
            }
            false
        };
        if b[p..].starts_with(b"pub") {
            p += 3;
            if p < b.len() && b[p] == b'(' {
                let mut d = 0i32;
                while p < b.len() {
                    if b[p] == b'(' {
                        d += 1;
                    } else if b[p] == b')' {
                        d -= 1;
                        if d == 0 {
                            p += 1;
                            break;
                        }
                    }
                    p += 1;
                }
            }
            while p < b.len() && b[p] == b' ' {
                p += 1;
            }
        }
        loop {
            if eat(b, &mut p, b"default") || eat(b, &mut p, b"const") || eat(b, &mut p, b"async") {
                continue;
            }
            if eat(b, &mut p, b"unsafe") {
                continue;
            }
            // `extern "C" fn`. The ABI string must be consumed by matching its
            // quotes, NOT by "skip alphanumerics" — the blanked source renders
            // it `extern " " ` and a character-class skip walks straight
            // through the `fn` that follows, silently dropping every
            // `extern` definition from the population.
            if b[p..].starts_with(b"extern") && (p + 6 >= b.len() || !is_ident(b[p + 6])) {
                p += 6;
                while p < b.len() && b[p] == b' ' {
                    p += 1;
                }
                if p < b.len() && b[p] == b'"' {
                    p += 1;
                    while p < b.len() && b[p] != b'"' {
                        p += 1;
                    }
                    if p < b.len() {
                        p += 1;
                    }
                }
                while p < b.len() && b[p] == b' ' {
                    p += 1;
                }
                continue;
            }
            break;
        }
        if !b[p..].starts_with(b"fn") {
            return None;
        }
        p += 2;
        if p >= b.len() || (b[p] != b' ' && b[p] != b'\n') {
            return None;
        }
        while p < b.len() && (b[p] == b' ' || b[p] == b'\n') {
            p += 1;
        }
        let s = p;
        if s >= b.len() || !is_ident_start(b[s]) {
            return None;
        }
        while p < b.len() && is_ident(b[p]) {
            p += 1;
        }
        Some((String::from_utf8_lossy(&b[s..p]).into_owned(), p))
    }

    /// All `fn` definitions in one blanked file.
    pub fn fn_defs(file: usize, blanked: &[u8], raw: &str) -> Vec<FnDef> {
        let spans = cfg_test_spans(blanked);
        let raw_lines: Vec<&str> = raw.lines().collect();
        let mut out = Vec::new();
        let mut line_no = 1usize;
        let mut i = 0usize;
        while i < blanked.len() {
            let line_start = i;
            let line_end = blanked[i..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(blanked.len(), |p| i + p);
            let mut p = line_start;
            while p < line_end && (blanked[p] == b' ' || blanked[p] == b'\t') {
                p += 1;
            }
            let indent = p - line_start;
            if p < line_end {
                if let Some((name, after)) = parse_fn_header(blanked, p) {
                    if let Some((bs, be)) = body_span(blanked, after) {
                        let sig = String::from_utf8_lossy(&blanked[after..bs]).into_owned();
                        let attrs = attrs_above(&raw_lines, line_no);
                        let testish = attrs.iter().any(|a| {
                            a.contains("cfg(test)")
                                || a.starts_with("#[test]")
                                || a.contains("cfg(all(test")
                        }) || spans.iter().any(|&(s, e)| s <= bs && be <= e);
                        out.push(FnDef {
                            file,
                            name,
                            line: line_no,
                            indent,
                            takes_registry: sig.contains("NativeMethodRegistry"),
                            syn_gated: attrs.iter().any(|a| a.contains("synthetic-jdk")),
                            testish,
                            start: bs,
                            end: be,
                        });
                    }
                }
            }
            i = line_end + 1;
            line_no += 1;
        }
        out
    }

    /// From just past the fn name, find the body `{ .. }`. Returns `None` for a
    /// bodyless declaration (a trait method), signalled by a `;` at paren
    /// depth 0.
    fn body_span(b: &[u8], from: usize) -> Option<(usize, usize)> {
        let mut d = 0i32;
        let mut j = from;
        let mut open = None;
        while j < b.len() {
            match b[j] {
                b'(' => d += 1,
                b')' => d -= 1,
                b';' if d == 0 => return None,
                b'{' if d == 0 => {
                    open = Some(j);
                    break;
                }
                _ => {}
            }
            j += 1;
        }
        let open = open?;
        match_brace(b, open).map(|e| (open, e))
    }

    /// `#[...]` attribute lines immediately above `line_no` (1-based), skipping
    /// doc comments and blanks.
    fn attrs_above(lines: &[&str], line_no: usize) -> Vec<String> {
        let mut out = Vec::new();
        let mut a = line_no as i64 - 2;
        while a >= 0 {
            let t = lines[a as usize].trim();
            if t.starts_with("#[") {
                out.push(t.to_string());
            } else if t.starts_with("//") || t.is_empty() {
                // keep walking
            } else {
                break;
            }
            a -= 1;
        }
        out
    }

    /// Identifiers in `b` that are immediately followed by `(` and are not a
    /// method call (`.foo(`) or a definition (`fn foo(`).
    pub fn called_names(b: &[u8], mut sink: impl FnMut(&[u8])) {
        let mut i = 0usize;
        while i < b.len() {
            if !is_ident_start(b[i]) {
                i += 1;
                continue;
            }
            let s = i;
            while i < b.len() && is_ident(b[i]) {
                i += 1;
            }
            let e = i;
            let prev = if s == 0 { b' ' } else { b[s - 1] };
            if prev == b'.' {
                continue;
            }
            let mut j = e;
            while j < b.len() && (b[j] == b' ' || b[j] == b'\n' || b[j] == b'\t' || b[j] == b'\r') {
                j += 1;
            }
            if j >= b.len() || b[j] != b'(' {
                continue;
            }
            // `fn name(` is a definition, not a call.
            let mut k = s;
            while k > 0 && (b[k - 1] == b' ' || b[k - 1] == b'\n') {
                k -= 1;
            }
            if k >= 2 && &b[k - 2..k] == b"fn" && (k == 2 || !is_ident(b[k - 3])) {
                continue;
            }
            sink(&b[s..e]);
        }
    }

    /// Collect every identifier in `b`, for the outside-the-crate reference
    /// scan. `b` must already be blanked.
    pub fn idents(b: &[u8], mut sink: impl FnMut(&[u8])) {
        let mut i = 0usize;
        while i < b.len() {
            if !is_ident_start(b[i]) {
                i += 1;
                continue;
            }
            let s = i;
            while i < b.len() && is_ident(b[i]) {
                i += 1;
            }
            sink(&b[s..i]);
        }
    }

    /// Every `.rs` file under `dir`, skipping VCS/build/agent directories.
    pub fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if p.is_dir() {
                // `.claude` holds sibling worktrees — whole extra copies of
                // this repo. Descending into one would double every count and
                // make the answer depend on which lanes happen to be running.
                if matches!(
                    name.as_ref(),
                    ".git" | "target" | "node_modules" | ".claude"
                ) {
                    continue;
                }
                rs_files(&p, out);
            } else if name.ends_with(".rs") {
                out.push(p);
            }
        }
    }
}

// ===========================================================================
// SECTION 3 — the analysis
// ===========================================================================

/// Everything the assertions read, computed once per test process.
///
/// `OnceLock` is safe here in a way it usually is not: this is a pure function
/// of the working tree, evaluated inside one `cargo test` process, so it cannot
/// latch a guess taken under conditions that later change.
struct Analysis {
    crate_files: usize,
    workspace_files: usize,
    fn_defs: usize,
    passes: usize,
    external_refs: usize,
    synthetic_overrides_body: usize,
    direct_children: BTreeSet<String>,
    shipping: BTreeSet<String>,
    synthetic_only: BTreeSet<String>,
    /// `name -> "file:line"` for every pass, for legible failure messages.
    where_defined: BTreeMap<String, String>,
    /// `name -> callers`, for the same reason.
    called_from: BTreeMap<String, BTreeSet<String>>,
}

fn analysis() -> &'static Analysis {
    static A: OnceLock<Analysis> = OnceLock::new();
    A.get_or_init(build_analysis)
}

fn build_analysis() -> Analysis {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src = manifest.join("src");
    let workspace = manifest
        .parent()
        .expect("native-builtins has a parent directory (the workspace root)")
        .to_path_buf();

    // --- 1. parse this crate ------------------------------------------------
    let mut paths = Vec::new();
    scan::rs_files(&src, &mut paths);
    paths.sort();
    let crate_files = paths.len();

    let mut files: Vec<scan::FileSrc> = Vec::with_capacity(crate_files);
    let mut fns: Vec<scan::FnDef> = Vec::new();
    for p in &paths {
        let raw = std::fs::read_to_string(p)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()));
        let blanked = scan::blank(raw.as_bytes());
        let idx = files.len();
        fns.extend(scan::fn_defs(idx, &blanked, &raw));
        let rel = p
            .strip_prefix(&manifest)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/");
        files.push(scan::FileSrc { rel, blanked });
    }

    // --- 2. the population: passes, keyed on the ARGUMENT -------------------
    let mut pass_names: BTreeSet<String> = BTreeSet::new();
    let mut where_defined: BTreeMap<String, String> = BTreeMap::new();
    let mut syn_gated: BTreeSet<String> = BTreeSet::new();
    for f in &fns {
        if !f.takes_registry {
            continue;
        }
        pass_names.insert(f.name.clone());
        where_defined
            .entry(f.name.clone())
            .or_insert_with(|| format!("native-builtins/{}:{}", files[f.file].rel, f.line));
        if f.syn_gated {
            syn_gated.insert(f.name.clone());
        }
    }
    let passes = pass_names.len();

    // --- 3. enclosing top-level fn for nested definitions -------------------
    let mut tops_by_file: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (i, f) in fns.iter().enumerate() {
        if f.indent == 0 {
            tops_by_file.entry(f.file).or_default().push(i);
        }
    }
    let enclosing: Vec<usize> = fns
        .iter()
        .enumerate()
        .map(|(i, f)| {
            if f.indent == 0 {
                return i;
            }
            tops_by_file
                .get(&f.file)
                .and_then(|tops| {
                    tops.iter()
                        .copied()
                        .find(|&t| fns[t].start <= f.start && f.end <= fns[t].end)
                })
                .unwrap_or(i)
        })
        .collect();

    // --- 4. edges -----------------------------------------------------------
    let mut calls_from: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut called_from: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    // Callees whose caller is not a registration pass at all — an ordinary
    // helper, a boot shim. Those are shipping roots.
    let mut non_pass_called: BTreeSet<String> = BTreeSet::new();
    for (i, f) in fns.iter().enumerate() {
        let o = enclosing[i];
        if f.testish || fns[o].testish {
            continue;
        }
        let body = &files[f.file].blanked[f.start..=f.end];
        let owner = fns[o].name.clone();
        let self_name = f.name.clone();
        let mut hits: BTreeSet<String> = BTreeSet::new();
        scan::called_names(body, |id| {
            let s = String::from_utf8_lossy(id);
            if pass_names.contains(s.as_ref()) && s != self_name && s != owner {
                hits.insert(s.into_owned());
            }
        });
        let caller_is_pass = f.takes_registry || fns[o].takes_registry;
        for h in hits {
            called_from.entry(h.clone()).or_default().insert(format!(
                "{owner} @ native-builtins/{}:{}",
                files[f.file].rel, f.line
            ));
            calls_from
                .entry(owner.clone())
                .or_default()
                .insert(h.clone());
            if !caller_is_pass {
                non_pass_called.insert(h);
            }
        }
    }

    // --- 5. references from outside this crate's src ------------------------
    let mut wpaths = Vec::new();
    scan::rs_files(&workspace, &mut wpaths);
    let mut external: BTreeSet<String> = BTreeSet::new();
    let mut workspace_files = 0usize;
    for p in &wpaths {
        let rel = p
            .strip_prefix(&workspace)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/");
        if rel.starts_with("native-builtins/src/") {
            continue;
        }
        // A call site in a test is not a shipping call site — treating one as
        // such is precisely how a test comes to cover an implementation the
        // shipping mode never runs.
        if rel
            .split('/')
            .any(|seg| matches!(seg, "tests" | "benches" | "examples" | "fuzz"))
        {
            continue;
        }
        workspace_files += 1;
        let Ok(raw) = std::fs::read_to_string(p) else {
            continue;
        };
        let mut blanked = scan::blank(raw.as_bytes());
        for (s, e) in scan::cfg_test_spans(&blanked) {
            for b in blanked[s..=e].iter_mut() {
                if *b != b'\n' {
                    *b = b' ';
                }
            }
        }
        scan::idents(&blanked, |id| {
            let s = String::from_utf8_lossy(id);
            if pass_names.contains(s.as_ref()) {
                external.insert(s.into_owned());
            }
        });
    }
    let external_refs = external.len();

    // --- 6. reachability ----------------------------------------------------
    let reach = |seeds: &BTreeSet<String>, block: &BTreeSet<String>| -> BTreeSet<String> {
        let mut seen: BTreeSet<String> = seeds.clone();
        let mut stack: Vec<String> = seeds.iter().cloned().collect();
        while let Some(n) = stack.pop() {
            if let Some(cs) = calls_from.get(&n) {
                for c in cs {
                    if block.contains(c) || seen.contains(c) {
                        continue;
                    }
                    seen.insert(c.clone());
                    stack.push(c.clone());
                }
            }
        }
        seen
    };

    let syn_seed: BTreeSet<String> = [SYNTHETIC_OVERRIDES.to_string()].into_iter().collect();
    let mut syn_reach = reach(&syn_seed, &BTreeSet::new());
    syn_reach.remove(SYNTHETIC_OVERRIDES);

    // `register_builtins` is ITSELF `#[cfg(feature = "synthetic-jdk")]`, is
    // `pub`, and is referenced from `vm/src/...`. Without excluding the gated
    // passes from the shipping roots AND from the shipping traversal it drags
    // all 463 synthetic-reachable names into `shipping`, and this whole gate
    // reports a vacuous zero. That is not hypothetical — it is what the first
    // run of this census did.
    let mut roots: BTreeSet<String> = external.union(&non_pass_called).cloned().collect();
    for g in &syn_gated {
        roots.remove(g);
    }
    let shipping = reach(&roots, &syn_gated);

    let synthetic_only: BTreeSet<String> = syn_reach.difference(&shipping).cloned().collect();

    let direct_all = calls_from
        .get(SYNTHETIC_OVERRIDES)
        .cloned()
        .unwrap_or_default();
    let direct_children: BTreeSet<String> = direct_all
        .iter()
        .filter(|n| synthetic_only.contains(*n))
        .cloned()
        .collect();

    let synthetic_overrides_body = fns
        .iter()
        .find(|f| f.name == SYNTHETIC_OVERRIDES && f.indent == 0)
        .map(|f| f.end - f.start)
        .unwrap_or(0);

    Analysis {
        crate_files,
        workspace_files,
        fn_defs: fns.len(),
        passes,
        external_refs,
        synthetic_overrides_body,
        direct_children,
        shipping,
        synthetic_only,
        where_defined,
        called_from,
    }
}

fn describe(a: &Analysis, n: &str) -> String {
    let at = a
        .where_defined
        .get(n)
        .map_or("<no definition found>".to_string(), Clone::clone);
    let from = a
        .called_from
        .get(n)
        .map(|s| s.iter().cloned().collect::<Vec<_>>().join("; "))
        .unwrap_or_else(|| "<no call site found>".to_string());
    format!("{n}\n        defined at {at}\n        called from {from}")
}

// ===========================================================================
// SECTION 4 — the assertions
// ===========================================================================

/// The scanner must be measuring something, and must discriminate.
///
/// Every stage is floored against a measured value, and the two controls pin
/// BOTH answers: `register_pe_panama` synthetic-only, `register_p64_hex_format`
/// not. A scanner that has quietly stopped resolving call sites answers
/// "synthetic-only" for everything and would pass a positive-only control.
#[test]
fn the_scanner_is_not_vacuous() {
    let a = analysis();

    println!(
        "registrar-reachability: {} crate files, {} workspace files, {} fn defs, \
         {} registration passes, {} external refs, {} direct synthetic-only families, \
         {} synthetic-only passes, {} shipping-reachable",
        a.crate_files,
        a.workspace_files,
        a.fn_defs,
        a.passes,
        a.external_refs,
        a.direct_children.len(),
        a.synthetic_only.len(),
        a.shipping.len(),
    );

    assert!(
        a.crate_files >= MIN_CRATE_FILES,
        "found only {} .rs files under native-builtins/src (floor {MIN_CRATE_FILES}); \
         the directory walk is broken and every count below it is meaningless",
        a.crate_files
    );
    assert!(
        a.workspace_files >= MIN_WORKSPACE_FILES,
        "found only {} .rs files outside native-builtins/src (floor {MIN_WORKSPACE_FILES}); \
         without the workspace scan nothing is a shipping root and EVERY pass looks \
         synthetic-only",
        a.workspace_files
    );
    assert!(
        a.fn_defs >= MIN_FN_DEFS,
        "parsed only {} fn definitions (floor {MIN_FN_DEFS}); the header parser is broken",
        a.fn_defs
    );
    assert!(
        a.passes >= MIN_PASSES,
        "found only {} registration passes (floor {MIN_PASSES}); the population is keyed on a \
         signature mentioning `NativeMethodRegistry` — if that type was renamed, re-point this",
        a.passes
    );
    assert!(
        a.synthetic_overrides_body >= MIN_SYNTHETIC_OVERRIDES_BODY,
        "`{SYNTHETIC_OVERRIDES}`'s body extracted as {} bytes (floor \
         {MIN_SYNTHETIC_OVERRIDES_BODY}); the brace scan is broken, so the direct-child set \
         is a fragment and the allow-list is being compared against nothing",
        a.synthetic_overrides_body
    );
    assert!(
        a.direct_children.len()
            >= MIN_DIRECT_CHILDREN.min(DELIBERATE_SYNTHETIC_ONLY_FAMILIES.len()),
        "`{SYNTHETIC_OVERRIDES}` has only {} synthetic-only direct children; the call scan is \
         broken",
        a.direct_children.len()
    );
    assert!(
        a.shipping.len() >= MIN_SHIPPING,
        "only {} passes are shipping-reachable (floor {MIN_SHIPPING}); the shipping closure \
         collapsed, which makes everything look synthetic-only",
        a.shipping.len()
    );
    assert!(
        a.external_refs >= MIN_EXTERNAL_REFS,
        "only {} pass names are referenced outside native-builtins/src (floor \
         {MIN_EXTERNAL_REFS}); the external scan found nothing to root the shipping closure on",
        a.external_refs
    );

    // --- the two-sided control -------------------------------------------
    assert!(
        a.synthetic_only.contains(CONTROL_POSITIVE),
        "POSITIVE CONTROL FAILED: `{CONTROL_POSITIVE}` is not reported synthetic-only. Either \
         it was promoted onto the shipping path — in which case delete it from \
         SYNTHETIC_ONLY_CLOSURE and DELIBERATE_SYNTHETIC_ONLY_FAMILIES and re-point this \
         control at another known instance — or the scanner has stopped discriminating and \
         every other assertion in this file is worthless.\n    {}",
        describe(a, CONTROL_POSITIVE)
    );
    assert!(
        !a.synthetic_only.contains(CONTROL_NEGATIVE),
        "NEGATIVE CONTROL FAILED: `{CONTROL_NEGATIVE}` is reported synthetic-only. It was \
         promoted onto the shipping path by W8-C15-2 — `register_hex_format_real_jdk_natives` \
         calls it LAST from `register_essential_natives_with_shims`, deliberately last because \
         `register()` is last-write-wins and the phases_late family would otherwise only win \
         in synthetic-jdk mode. If that call was removed, this is the regression; if the \
         scanner answers 'synthetic-only' for everything, it has stopped discriminating.\n    {}",
        describe(a, CONTROL_NEGATIVE)
    );
}

/// The allow-list must stay a list of justified cases, not a dumping ground.
#[test]
fn the_allow_list_is_well_formed() {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for (name, reason) in DELIBERATE_SYNTHETIC_ONLY_FAMILIES {
        assert!(
            seen.insert(name),
            "`{name}` is listed twice in DELIBERATE_SYNTHETIC_ONLY_FAMILIES"
        );
        assert!(
            reason.len() >= 40,
            "`{name}`'s reason is {} chars: {reason:?}. A row here asserts that --jdk-only is \
             not missing a capability; say which of the four verdicts it is (SHIPPING TWIN / \
             REAL-JDK BYTECODE / APP/ABSENT / TOMBSTONE) and what was measured.",
            reason.len()
        );
    }

    let closure: BTreeSet<&str> = SYNTHETIC_ONLY_CLOSURE.iter().copied().collect();
    assert_eq!(
        closure.len(),
        SYNTHETIC_ONLY_CLOSURE.len(),
        "SYNTHETIC_ONLY_CLOSURE contains duplicates"
    );
    for (name, _) in DELIBERATE_SYNTHETIC_ONLY_FAMILIES {
        assert!(
            closure.contains(name),
            "`{name}` is allow-listed as a deliberate synthetic-only FAMILY but is absent from \
             SYNTHETIC_ONLY_CLOSURE. The family list is a subset of the closure by \
             construction; the two were edited apart."
        );
    }
}

/// A new synthetic-only FAMILY — a fresh direct call in
/// `register_synthetic_overrides` that nothing on the shipping side reaches —
/// must be a deliberate, justified decision.
#[test]
fn no_new_synthetic_only_family() {
    let a = analysis();
    let allowed: BTreeSet<&str> = DELIBERATE_SYNTHETIC_ONLY_FAMILIES
        .iter()
        .map(|(n, _)| *n)
        .collect();
    let observed: BTreeSet<&str> = a.direct_children.iter().map(String::as_str).collect();

    let unlisted: Vec<String> = observed
        .difference(&allowed)
        .map(|n| describe(a, n))
        .collect();
    let stale: Vec<&&str> = allowed.difference(&observed).collect();

    println!(
        "registrar-reachability(families): {} synthetic-only direct children, {} allow-listed",
        observed.len(),
        allowed.len()
    );

    assert!(
        unlisted.is_empty(),
        "{} registrar family/families are reachable ONLY through \
         `{SYNTHETIC_OVERRIDES}` and are not listed as deliberate:\n    {}\n\n\
         `register_builtins` -> `{SYNTHETIC_OVERRIDES}` is \
         `#[cfg(feature = \"synthetic-jdk\")]` and `synthetic-jdk` is in no crate's default \
         feature set, so this code is NOT IN THE SHIPPING BINARY. In `--jdk-only` these \
         `register(...)` calls do not exist to be declined. Before you add a row to \
         DELIBERATE_SYNTHETIC_ONLY_FAMILIES, answer the question PER MODE — what serves these \
         classes in real-JDK mode? Three different answers:\n\
         \x20 * real JDK bytecode serves them (check the class exists in JDK 25 and declares \
         no `native` method) — deliberate, say so;\n\
         \x20 * nothing serves them — a `--jdk-only` capability gap, wire the registrar onto \
         the shipping path instead of allow-listing it;\n\
         \x20 * a different registrar serves them in the shipping mode — then there are two \
         implementations that will drift, and `register()` is last-write-wins, so say which \
         one wins and whether they agree. That is the `register_pe_panama` / `structLayout` \
         case exactly.\n\
         Do NOT fix this by narrowing the registration to a runtime flag: doing that once \
         removed a class from synthetic-jdk mode entirely, where its stub has no method \
         bodies at all.\n\
         See docs/known-issues/jdk-only/W7-5-registrars-that-never-shipped.md §3 for the four \
         reasons gating can be right, and this lane's F34-1 record for the per-family census.",
        unlisted.len(),
        unlisted.join("\n    ")
    );

    assert!(
        stale.is_empty(),
        "{} allow-listed synthetic-only family/families are no longer synthetic-only: {:?}\n\
         This is GOOD NEWS that still has to be recorded: something wired them onto the \
         shipping path (or deleted them). Remove them from \
         DELIBERATE_SYNTHETIC_ONLY_FAMILIES and from SYNTHETIC_ONLY_CLOSURE. A stale \
         exemption is a hole — it would let the family drift back to synthetic-only without \
         this gate saying a word.",
        stale.len(),
        stale
    );
}

/// A registrar that today has a shipping call site and tomorrow loses it must
/// not slip in unnoticed.
///
/// This is the assertion [`no_new_synthetic_only_family`] cannot make. Such a
/// pass is usually buried inside an already-allow-listed family's subtree —
/// `register_p64_hex_format` sits under `register_phase64_natives`, which is
/// allow-listed — so a family-level check inherits the exemption and says
/// nothing. Only an exact set over the whole transitive closure catches it.
#[test]
fn no_registrar_silently_orphaned_into_the_synthetic_arm() {
    let a = analysis();
    let pinned: BTreeSet<&str> = SYNTHETIC_ONLY_CLOSURE.iter().copied().collect();
    let observed: BTreeSet<&str> = a.synthetic_only.iter().map(String::as_str).collect();

    let entered: Vec<String> = observed
        .difference(&pinned)
        .map(|n| describe(a, n))
        .collect();
    let left: Vec<&&str> = pinned.difference(&observed).collect();

    println!(
        "registrar-reachability(closure): {} synthetic-only passes observed, {} pinned",
        observed.len(),
        pinned.len()
    );

    assert!(
        entered.is_empty(),
        "{} registration pass(es) became synthetic-only:\n    {}\n\n\
         Either they are new, or — the case this assertion exists for — they HAD a shipping \
         call site and lost it, which silently moves the implementation the shipping modes \
         run. That is the `register_pe_panama` shape: one call site, inside \
         `{SYNTHETIC_OVERRIDES}`, and two modes running different code with every test on \
         the wrong one.\n\
         If the move is deliberate, add the name here and, if it is a direct child of \
         `{SYNTHETIC_OVERRIDES}`, to DELIBERATE_SYNTHETIC_ONLY_FAMILIES with a measured \
         reason. If it is not deliberate, restore the shipping call site — and mind the \
         ORDER, because `register()` is last-write-wins with no unregister API.",
        entered.len(),
        entered.join("\n    ")
    );

    assert!(
        left.is_empty(),
        "{} pinned synthetic-only pass(es) are no longer synthetic-only: {:?}\n\
         Something reached them from the shipping side, or they were deleted or renamed. \
         Shrink SYNTHETIC_ONLY_CLOSURE in the same commit. This list is a ratchet in BOTH \
         directions on purpose: a name left behind after a promotion is an exemption for a \
         registrar that could drift back with nothing watching.",
        left.len(),
        left
    );
}
