// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The router census (interpreter round i6 wave 118, lane L1; step 1 of
//! `docs/feature-designs/native-dispatch-architecture.md`; closed out by the
//! i74-L1 page's "Closure (2026-11-02)").
//!
//! A "router" is a registered native that, for a real receiver, does nothing
//! but run the JDK's (or the library's) own bytecode again through
//! `NativeContext::invoke_special_bytecode_only` (or its `_by_class_id` /
//! `_hinted` forms). In `--compatible` it is a native door plus a second
//! dispatch per call; in `--real-jdk` the bytecode wins anyway. Waves 74-82
//! turned the whole-class j.u.c ones into family rows; this census says
//! where the rest are, per file, per class and per BINDING RULE, which is
//! Java-visible (wave 75, `i75-L1-jdk-only-copyonwritearrayset-subclass-super-add-loses-the-element`):
//!
//! * `base <class>`: the router names the class whose body it runs (a
//!   literal, or a `const &str` of the same file or, when every definition in
//!   the scanned crates agrees, of another): a subclass's `super.m()` runs
//!   that class's body, as `invokespecial` does;
//! * `receiver in fn <f>`: the class is computed at run time (the receiver's
//!   class, a descriptor's owner): the router in function `f` binds on what it
//!   is handed, and a subclass's bridge method can re-enter it.
//!
//! The census is a SOURCE scan (host- and feature-independent): every call of
//! the three methods in the `src` of every `native-*` crate that registers
//! natives, outside `//` comments. [`FROZEN`] holds it with zero slack, so a
//! new router (or a router turned into a family row) is a named line in the
//! diff, not a grep a later wave repeats by hand. Re-freeze by pasting the
//! printed literal (`cargo test -p cratonvm-native-builtins --test
//! router_census -- --nocapture`), review the new row (its verdict is
//! printed as `"open"`, which [`every_router_row_has_a_reviewed_verdict`]
//! refuses), and say on the i74-L1 page why the row moved.
//!
//! Every row carries its REVIEWED VERDICT (the fourth field): `kind` or
//! `kind:note`. The kinds:
//!
//! * `family:<row>`: a built `COMPAT_FAMILIES` row takes (or, default off,
//!   will take) the class's routers; `<row>` is its `HotSpotAlignment` token;
//! * `covered:<row>`: the class already belongs to an existing row (or the
//!   native is a door of one), so a second row would take nothing;
//! * `loader`, `invoke`: loader (L5) and method-handle (L4) territory, reviewed
//!   by their lanes; not a router a family row may take;
//! * `carrier`: the native carries VM state the JDK body lacks (a VM-minted
//!   receiver), so the router is half of an implementation;
//! * `gated`: registered only under `--real-jdk`, or behind an opt-in switch;
//! * `decline`: the native implements the call and runs the body only on a
//!   decline / fault / refusal arm (a reviewed performance `Intrinsic`, an
//!   exception-frames arm); not a per-call router;
//! * `wrapper`: the native adds work around the body;
//! * `app`: an application / library class with no JDK retirement (a family
//!   row only adopts JDK retirements);
//! * `synthetic`: the registrar is compiled only into synthetic-jdk builds;
//! * `dead`: a registration a real JDK never reaches (a deletion candidate).
//!
//! No row is `"open"`: step 2 of the page has been run over the whole census.
//!
//! The VIRTUAL form (`invoke_virtual_bytecode_only`, which re-dispatches on
//! the RECEIVER's class) is the form wave 75 and `delegate_to_real_bytecode`
//! warn about: a subclass override that calls `super.m()` reaches the native
//! again and bounces. It is counted per file in [`VIRTUAL_FROZEN`] so a new
//! site is a named line too; the review rule and the sites still to read are
//! in `i74x-L1-receiver-class-routers-super-bounce-audit-20261102.md`.

use cratonvm_types::HotSpotAlignment as Row;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The crates scanned: every `native-*` crate that registers natives.
const CRATES: &[&str] = &[
    "native-builtins",
    "native-collections",
    "native-io",
    "native-awt",
    "native-builtins-crypto",
    "native-builtins-security",
];

/// The crates whose `src` must exist (an empty scan is a broken path, not a
/// clean census).
const REQUIRED_CRATES: &[&str] = &["native-builtins", "native-collections", "native-io"];

/// The verdict kinds (see the module doc).
const KINDS: &[&str] = &[
    "family",
    "covered",
    "loader",
    "invoke",
    "carrier",
    "gated",
    "decline",
    "wrapper",
    "app",
    "synthetic",
    "dead",
];

/// `(file, binding, calls, verdict)`, sorted: the census at the 2026-11-02
/// freeze (145 calls: 124 base-class, 21 receiver-bound).
const FROZEN: &[(&str, &str, usize, &str)] = &[
    ("native-builtins/src/antlr_intrinsics.rs", "receiver in fn antlr_null_case_runs_bytecode", 1, "app"),
    ("native-builtins/src/antlr_intrinsics.rs", "receiver in fn native_antlr_atn_config_set_add", 1, "app"),
    ("native-builtins/src/apps_h2.rs", "base org/h2/expression/ExpressionColumn", 1, "app"),
    ("native-builtins/src/charset.rs", "base java/lang/String", 1, "family:jdk-string-get-bytes"),
    ("native-builtins/src/classloader/adopt_add_class_orphan_by_id.rs", "base java/lang/ClassLoader", 4, "loader:jdk-url-class-loaders"),
    ("native-builtins/src/classloader/builtin_classloader_find_resource_as.rs", "base jdk/internal/loader/Resource", 1, "loader:jdk-url-class-loaders"),
    ("native-builtins/src/classloader/builtin_classloader_find_resource_as.rs", "base jdk/internal/loader/URLClassPath$Loader", 1, "loader:jdk-url-class-loaders"),
    ("native-builtins/src/classloader/load_class_override_memo_key.rs", "receiver in fn call_loader_add_class_hinted (hinted)", 1, "gated:opt-in hinted addClass upcall (i7 w135)"),
    ("native-builtins/src/classloader_real.rs", "base jdk/internal/loader/BuiltinClassLoader", 2, "loader:--real-jdk built-in loader rows (zt define path)"),
    ("native-builtins/src/date_format_fast.rs", "receiver in fn parse_via_bytecode (by_class_id)", 1, "decline:date-format parse fallback"),
    ("native-builtins/src/http_url_connection.rs", "receiver in fn https_forward_to_super", 1, "carrier:the VM's https connection state"),
    ("native-builtins/src/jca/message_digest.rs", "base java/security/MessageDigest", 1, "covered:jdk-providers"),
    ("native-builtins/src/jdbc.rs", "base org/apache/derby/iapi/jdbc/InternalDriver", 1, "app"),
    ("native-builtins/src/lang_class/classloader_packages_synced.rs", "base java/lang/ClassLoader", 2, "loader:jdk-url-class-loaders"),
    ("native-builtins/src/lang_class/declaring_class_loader_aware.rs", "receiver in fn annotated_type_real_body", 1, "gated:--real-jdk branch, --compatible never takes it"),
    ("native-builtins/src/lang_invoke/adapters/method_finder_mint_file.rs", "base java/lang/invoke/DirectMethodHandle", 1, "invoke:jdk-invoke (L4)"),
    ("native-builtins/src/lang_invoke/adapters/method_finder_mint_file.rs", "base java/lang/invoke/Invokers", 1, "invoke:jdk-invoke (L4)"),
    ("native-builtins/src/lang_invoke/adapters/method_finder_mint_file.rs", "base java/lang/invoke/MethodHandleImpl", 1, "invoke:jdk-invoke (L4)"),
    ("native-builtins/src/lang_invoke/adapters/mh_bindto_prelink_bound_field.rs", "base java/lang/invoke/MethodHandle", 2, "invoke:jdk-invoke (L4)"),
    ("native-builtins/src/lang_invoke/adapters/mh_bindto_prelink_bound_field.rs", "base java/lang/invoke/MethodType", 1, "invoke:jdk-invoke (L4)"),
    ("native-builtins/src/lang_invoke/adapters/mh_bindto_prelink_bound_field.rs", "receiver in fn mt_make_impl_jdk_body (hinted)", 1, "invoke:jdk-invoke (L4)"),
    ("native-builtins/src/lang_invoke/combinators.rs", "base java/lang/invoke/InvokerBytecodeGenerator", 1, "invoke:jdk-invoke (L4)"),
    ("native-builtins/src/lang_invoke/combinators.rs", "base java/lang/invoke/MemberName", 1, "invoke:jdk-invoke (L4)"),
    ("native-builtins/src/lang_invoke/combinators.rs", "base java/lang/invoke/VarHandle", 2, "invoke:jdk-invoke (L4)"),
    ("native-builtins/src/lang_invoke/member_name/mh_foreign_door_type.rs", "base java/lang/invoke/LambdaMetafactory", 1, "invoke:jdk-invoke (L4)"),
    ("native-builtins/src/lang_invoke/member_name/mh_foreign_door_type.rs", "base java/lang/invoke/MethodHandle", 1, "invoke:jdk-invoke (L4)"),
    ("native-builtins/src/lang_invoke/member_name/vh_indirect_mode_handle.rs", "base java/lang/invoke/MethodHandles", 1, "invoke:jdk-invoke (L4)"),
    ("native-builtins/src/lang_invoke/var_handles/invoke_reference_cast_refusal.rs", "base java/lang/invoke/VarHandle", 2, "invoke:jdk-invoke (L4)"),
    ("native-builtins/src/lang_math.rs", "base java/lang/Math", 1, "decline:zero-divisor arm only"),
    ("native-builtins/src/lang_math.rs", "receiver in fn number_parse_failed", 1, "decline:failing parse only, CRATONVM_NUMBER_PARSE_RAISE_JDK_BODY"),
    ("native-builtins/src/lang_misc.rs", "receiver in fn null_arg_runs_shadowed_bytecode", 1, "decline:null-argument and refusal arms of library shadows"),
    ("native-builtins/src/lang_misc.rs", "receiver in fn shadow_refusal_runs_bytecode", 1, "decline:null-argument and refusal arms of library shadows"),
    ("native-builtins/src/lang_reflect.rs", "base sun/reflect/generics/reflectiveObjects/TypeVariableImpl", 1, "family:jdk-type-variable-impl"),
    ("native-builtins/src/lang_stackwalker.rs", "base java/lang/StackStreamFactory$StackFrameBuffer", 1, "gated:--real-jdk Intrinsic, CRATONVM_STACKWALK_JDK_PIPELINE_BATCHED"),
    ("native-builtins/src/lang_string/decode_string_chars.rs", "base java/lang/String", 1, "decline:fault and decline arms of String/StringBuilder natives"),
    ("native-builtins/src/lang_string/decode_string_chars.rs", "receiver in fn sb_append_object_through_bytecode", 1, "decline:fault and decline arms of String/StringBuilder natives"),
    ("native-builtins/src/lang_string/exact_regex_replace_units.rs", "base java/lang/String", 3, "decline:fault and decline arms of String/StringBuilder natives"),
    ("native-builtins/src/lang_string/fmt_temporal_fault_char.rs", "base java/lang/String", 2, "decline:fault and decline arms of String/StringBuilder natives"),
    ("native-builtins/src/lang_string/native_sb_get_chars.rs", "receiver in fn sb_raise_through_builder_body", 1, "decline:fault and decline arms of String/StringBuilder natives"),
    ("native-builtins/src/lib_parts/essential_dur_parse.rs", "receiver in fn delegate_to_real_bytecode", 1, "app:jboss-logmanager and juli handler shims"),
    ("native-builtins/src/locale_resources.rs", "base java/util/ResourceBundle", 1, "covered:jdk-locale-providers"),
    ("native-builtins/src/logging_shims.rs", "base org/slf4j/impl/StaticLoggerBinder", 1, "app"),
    ("native-builtins/src/logging_shims.rs", "base org/slf4j/impl/StaticMDCBinder", 1, "app"),
    ("native-builtins/src/lucene_es.rs", "base java/util/concurrent/AbstractExecutorService", 2, "covered:jdk-thread-pools"),
    ("native-builtins/src/lucene_es.rs", "base java/util/concurrent/ThreadPoolExecutor", 1, "covered:jdk-thread-pools"),
    ("native-builtins/src/net_phase_e/register_re4_url_http.rs", "base java/net/JarURLConnection", 1, "carrier:VM-minted handler-less URLs and jar connections"),
    ("native-builtins/src/net_phase_e/register_re4_url_http.rs", "base java/net/URL", 3, "carrier:VM-minted handler-less URLs and jar connections"),
    ("native-builtins/src/net_phase_e/register_re4_url_http.rs", "base java/net/URLConnection", 2, "carrier:VM-minted handler-less URLs and jar connections"),
    ("native-builtins/src/net_phase_e/register_re4_url_http.rs", "base org/springframework/boot/web/reactive/context/ReactiveWebServerApplicationContext", 1, "app"),
    ("native-builtins/src/net_phase_e/register_re4_url_http.rs", "base org/springframework/boot/web/server/servlet/context/ServletWebServerApplicationContext", 1, "app"),
    ("native-builtins/src/net_phase_e/register_re4_url_http.rs", "base org/springframework/boot/web/servlet/context/ServletWebServerApplicationContext", 1, "app"),
    ("native-builtins/src/net_phase_e/register_re4_url_http.rs", "base org/springframework/data/redis/core/RedisAccessor", 1, "app"),
    ("native-builtins/src/net_phase_e/register_re4_url_http.rs", "base org/springframework/data/redis/listener/RedisMessageListenerContainer", 1, "app"),
    ("native-builtins/src/net_phase_e/register_re4_url_http.rs", "base org/springframework/session/data/redis/RedisOperationsSessionRepository", 2, "app"),
    ("native-builtins/src/net_phase_e/register_re4_url_http.rs", "base org/springframework/session/data/redis/config/annotation/web/http/RedisHttpSessionConfiguration$EnableRedisKeyspaceNotificationsInitializer", 1, "app"),
    ("native-builtins/src/net_phase_e/register_re4_url_http.rs", "base org/springframework/util/ClassUtils", 1, "app"),
    ("native-builtins/src/net_phase_e/register_re4_url_http.rs", "base org/springframework/web/context/support/WebApplicationContextUtils$FacesDependencyRegistrar", 1, "app"),
    ("native-builtins/src/orm_hibernate.rs", "receiver in fn hibernate_null_required_arg_runs_bytecode", 1, "app"),
    ("native-builtins/src/orm_hibernate.rs", "receiver in fn hibernate_null_required_arg_runs_bytecode (by_class_id)", 1, "app"),
    ("native-builtins/src/panama/pe_swap_width.rs", "base jdk/internal/foreign/AbstractMemorySegmentImpl", 1, "decline:helper call of a different triple"),
    ("native-builtins/src/phases_early/p54_huc_do_request_body.rs", "base java/lang/StringLatin1", 1, "decline:fault arm only"),
    ("native-builtins/src/phases_early/p54_huc_do_request_body.rs", "base jdk/internal/util/ArraysSupport", 1, "decline:fault arm only"),
    ("native-builtins/src/phases_early/take_inherited_for_thread_object.rs", "base java/lang/ThreadLocal", 3, "gated:--real-jdk Intrinsic, CRATONVM_TL_REAL_FAST"),
    ("native-builtins/src/phases_late/bouncycastle/bc_md5_slots.rs", "base org/bouncycastle/crypto/digests/GeneralDigest", 1, "app"),
    ("native-builtins/src/phases_late/bouncycastle/bc_md5_slots.rs", "base org/bouncycastle/crypto/generators/Argon2BytesGenerator", 5, "app"),
    ("native-builtins/src/phases_late/bouncycastle/bc_md5_slots.rs", "base org/bouncycastle/crypto/generators/Argon2BytesGenerator$FixedBlockPool", 2, "app"),
    ("native-builtins/src/phases_late/bouncycastle/bc_md5_slots.rs", "base org/springframework/security/crypto/bcrypt/BCrypt", 1, "app"),
    ("native-builtins/src/phases_late/bouncycastle/bc_whirlpool_tables.rs", "base org/bouncycastle/crypto/modes/CBCBlockCipher", 1, "app"),
    ("native-builtins/src/phases_late/bouncycastle/bc_whirlpool_tables.rs", "receiver in fn bc_aes_native_ctor", 1, "app"),
    ("native-builtins/src/phases_late/concurrent.rs", "base java/lang/Thread", 2, "carrier:inheritable thread-local side table"),
    ("native-builtins/src/phases_late/concurrent.rs", "base java/util/concurrent/ThreadPoolExecutor", 1, "covered:jdk-thread-pools"),
    ("native-builtins/src/phases_late/foreign_ffm.rs", "base jdk/internal/foreign/layout/AbstractLayout", 1, "covered:jdk-foreign"),
    ("native-builtins/src/phases_late/jar_manifest.rs", "base org/springframework/boot/loader/launch/JarFileArchive", 1, "app"),
    ("native-builtins/src/phases_late/nio_file/register_p70_file_attributes.rs", "base java/nio/file/attribute/PosixFilePermissions", 1, "dead:synthetic image only, bytecode wins on a real JDK"),
    ("native-builtins/src/phases_late/nio_file/register_phase57_natives.rs", "receiver in fn yield_to_foreign_path", 1, "covered:jdk-archive-fs"),
    ("native-builtins/src/phases_late/nio_file/register_phase57_nio_file_c.rs", "base java/nio/file/Path", 1, "dead:synthetic image only, bytecode wins on a real JDK"),
    ("native-builtins/src/phases_late/ssl_security/jdk_body_for_engine_subclass.rs", "base javax/net/ssl/SSLEngine", 1, "carrier:the VM's own engines"),
    ("native-builtins/src/phases_late/ssl_security/register_p68_ssl.rs", "base javax/net/ssl/KeyManagerFactory", 1, "decline:KeyManagerFactory fallback"),
    ("native-builtins/src/phases_late/zip_streams.rs", "base java/util/zip/Deflater", 1, "synthetic:register_synthetic_overrides, cfg synthetic-jdk"),
    ("native-builtins/src/phases_late/zip_streams.rs", "base java/util/zip/GZIPInputStream", 1, "synthetic:register_synthetic_overrides, cfg synthetic-jdk"),
    ("native-builtins/src/phases_late/zip_streams.rs", "base java/util/zip/GZIPOutputStream", 1, "synthetic:register_synthetic_overrides, cfg synthetic-jdk"),
    ("native-builtins/src/phases_late/zip_streams.rs", "base java/util/zip/Inflater", 1, "synthetic:register_synthetic_overrides, cfg synthetic-jdk"),
    ("native-builtins/src/preconditions.rs", "receiver in fn raise_through_bytecode", 1, "decline:raise path only"),
    ("native-builtins/src/regex_matcher.rs", "base java/util/regex/Matcher", 1, "decline:fast-path bail, Matcher is final"),
    ("native-builtins/src/regex_matcher.rs", "base java/util/regex/Pattern", 1, "decline:real-layout compile runs the public JDK body to preserve exception frames"),
    ("native-builtins/src/scoped_twins.rs", "base jdk/internal/misc/ScopedMemoryAccess", 1, "wrapper:runs the body inside an access window"),
    ("native-builtins/src/securerandom.rs", "base java/security/SecureRandom", 1, "decline:provider lookup and subclass-draw arms"),
    ("native-builtins/src/securerandom.rs", "base java/util/Random", 1, "decline:provider lookup and subclass-draw arms"),
    ("native-builtins/src/util_concurrent_ext/register_queue_bridge_natives.rs", "base java/util/concurrent/locks/ReentrantLock", 1, "decline:no-JIT Intrinsic miss arm"),
    ("native-builtins/src/util_concurrent_ext/register_queue_bridge_natives.rs", "receiver in fn rl_fast_jdk_body (hinted)", 1, "decline:no-JIT Intrinsic miss arm"),
    ("native-builtins/src/util_concurrent_ext/register_t31_concurrent_extras.rs", "base java/util/concurrent/AbstractExecutorService", 2, "covered:jdk-thread-pools"),
    ("native-builtins/src/util_concurrent_ext/register_t31_concurrent_extras.rs", "base java/util/concurrent/ThreadPoolExecutor", 3, "covered:jdk-thread-pools"),
    ("native-builtins/src/vector_support_intrinsics.rs", "receiver in fn template_fallback", 1, "decline:vector template miss arm"),
    ("native-builtins/src/zip_real.rs", "base java/util/zip/ZipCoder$UTF8ZipCoder", 1, "decline:Intrinsic miss and fault arms"),
    ("native-builtins/src/zip_real.rs", "base java/util/zip/ZipFile$Source", 2, "decline:Intrinsic miss and fault arms"),
    ("native-builtins/src/zip_real.rs", "base java/util/zip/ZipUtils", 3, "decline:Intrinsic miss and fault arms"),
    ("native-collections/src/array_list.rs", "receiver in fn try_delegate_real_collection_args", 1, "covered:jdk-array-list (interface doors)"),
    ("native-collections/src/concurrent_hash_map/chm_key_hash.rs", "receiver in fn chm_door_runs_jdk_body", 1, "covered:jdk-concurrent-hash-map"),
    ("native-collections/src/concurrent_queue.rs", "base java/util/concurrent/CompletableFuture", 1, "covered:jdk-completable-future"),
    ("native-collections/src/concurrent_queue.rs", "base java/util/concurrent/ThreadPoolExecutor", 1, "covered:jdk-thread-pools"),
    ("native-collections/src/deque.rs", "base java/util/ArrayDeque", 1, "decline:copyElements tail of the native constructor and addAll"),
    ("native-collections/src/hash_map.rs", "base java/util/HashMap", 3, "covered:jdk-hash-collections"),
    ("native-collections/src/hash_map.rs", "base java/util/HashMap$TreeNode", 6, "covered:jdk-hash-collections"),
    ("native-collections/src/hash_set.rs", "base java/util/concurrent/CopyOnWriteArraySet", 1, "family:jdk-copy-on-write-set"),
    ("native-collections/src/linked_hash_map.rs", "base java/util/HashMap$TreeNode", 1, "covered:jdk-hash-collections"),
    ("native-io/src/file_channel_fast_read.rs", "base java/nio/channels/spi/AbstractInterruptibleChannel", 1, "decline:refusal and fault arms of the fast channel natives"),
    ("native-io/src/file_channel_fast_read.rs", "base sun/nio/ch/FileChannelImpl", 2, "decline:refusal and fault arms of the fast channel natives"),
    ("native-io/src/lib_parts/native_sr_skip.rs", "base java/nio/file/Files", 1, "decline:refusal and fault arms of the fast channel natives"),
    ("native-io/src/lib_parts/os_acquire_lock.rs", "base java/nio/MappedByteBuffer", 1, "carrier:VM-minted mapped buffers"),
];

/// `(file, calls)`: the sites of `invoke_virtual_bytecode_only(` (the
/// receiver-class form), outside `//` comments, at the 2026-11-02 freeze.
const VIRTUAL_FROZEN: &[(&str, usize)] = &[
    ("native-awt/src/natives.rs", 1),
    ("native-builtins/src/cglib_enhancer.rs", 1),
    (
        "native-builtins/src/classloader/adopt_add_class_orphan_by_id.rs",
        1,
    ),
    (
        "native-builtins/src/classloader/builtin_classloader_find_resource_as.rs",
        1,
    ),
    (
        "native-builtins/src/classloader/load_class_override_memo_key.rs",
        2,
    ),
    ("native-builtins/src/classloader_real.rs", 3),
    ("native-builtins/src/date_format_fast.rs", 2),
    ("native-builtins/src/http_url_connection.rs", 2),
    ("native-builtins/src/jboss_jdkspecific.rs", 1),
    (
        "native-builtins/src/lang_class/declaring_class_loader_aware.rs",
        2,
    ),
    (
        "native-builtins/src/lang_class/forget_lang_class_keys.rs",
        1,
    ),
    (
        "native-builtins/src/lang_class/wf7_synthesise_entry_class_if.rs",
        1,
    ),
    (
        "native-builtins/src/lang_invoke/adapters/method_finder_mint_file.rs",
        3,
    ),
    (
        "native-builtins/src/lang_invoke/adapters/mh_bindto_prelink_bound_field.rs",
        2,
    ),
    (
        "native-builtins/src/lang_invoke/adapters/mh_invoke_with_arguments_list_door.rs",
        2,
    ),
    ("native-builtins/src/lang_invoke/call_site.rs", 1),
    (
        "native-builtins/src/lang_invoke/member_name/mh_foreign_door_type.rs",
        5,
    ),
    (
        "native-builtins/src/lang_invoke/member_name/vh_indirect_mode_handle.rs",
        1,
    ),
    (
        "native-builtins/src/lang_invoke/var_handles/mh_real_field_converting_serve.rs",
        2,
    ),
    (
        "native-builtins/src/lang_invoke/var_handles/native_record_support.rs",
        2,
    ),
    ("native-builtins/src/lib_parts/dbg_toarray_enabled.rs", 1),
    ("native-builtins/src/lib_parts/essential_dur_parse.rs", 1),
    (
        "native-builtins/src/lib_parts/essential_natives_shims_a.rs",
        2,
    ),
    (
        "native-builtins/src/lib_parts/native_printwriter_flush.rs",
        1,
    ),
    ("native-builtins/src/logging_shims.rs", 3),
    ("native-builtins/src/logmanager/per_vm_table.rs", 2),
    ("native-builtins/src/messaging_shims.rs", 2),
    ("native-builtins/src/net_phase_e/header.rs", 1),
    ("native-builtins/src/panama/alloc_segment_carrier.rs", 1),
    (
        "native-builtins/src/phases_early/register_synthetic_socket_stubs.rs",
        1,
    ),
    ("native-builtins/src/phases_late.rs", 2),
    (
        "native-builtins/src/phases_late/bouncycastle/bc_md5_slots.rs",
        8,
    ),
    (
        "native-builtins/src/phases_late/bouncycastle/bc_whirlpool_tables.rs",
        2,
    ),
    ("native-builtins/src/phases_late/concurrent.rs", 2),
    ("native-builtins/src/phases_late/foreign_ffm.rs", 3),
    ("native-builtins/src/phases_late/jar_manifest.rs", 1),
    (
        "native-builtins/src/phases_late/nio_file/p57_line_separator.rs",
        1,
    ),
    (
        "native-builtins/src/phases_late/nio_file/p57_resolve_paths.rs",
        1,
    ),
    (
        "native-builtins/src/phases_late/nio_file/register_phase57_natives.rs",
        4,
    ),
    (
        "native-builtins/src/phases_late/nio_file/register_phase57_nio_file_a.rs",
        4,
    ),
    (
        "native-builtins/src/phases_late/nio_file/register_phase57_nio_file_c.rs",
        3,
    ),
    (
        "native-builtins/src/phases_late/ssl_security/register_p68_ssl.rs",
        8,
    ),
    ("native-builtins/src/phases_late/zip_streams.rs", 47),
    ("native-builtins/src/properties_sidetable.rs", 2),
    ("native-builtins/src/reflect_annotations.rs", 1),
    ("native-builtins/src/regex_matcher.rs", 1),
    ("native-builtins/src/spring_startup_bootstrap.rs", 2),
    ("native-builtins/src/test_frameworks.rs", 3),
    (
        "native-builtins/src/util_concurrent_ext/register_t31_concurrent_extras.rs",
        2,
    ),
    ("native-collections/src/array_list.rs", 2),
    ("native-collections/src/concurrent_queue.rs", 3),
    ("native-collections/src/iterators.rs", 1),
    ("native-collections/src/linked_list.rs", 1),
    ("native-io/src/async_socket.rs", 2),
    ("native-io/src/direct_buffer.rs", 14),
    ("native-io/src/lib_parts/native_bais_read_byte_array.rs", 1),
    ("native-io/src/lib_parts/native_noop.rs", 1),
    ("native-io/src/lib_parts/wrap_completed_future.rs", 1),
    ("native-io/src/socket_channel.rs", 1),
];

const NEEDLE: &str = "invoke_special_bytecode_only";
const VIRTUAL_NEEDLE: &str = "invoke_virtual_bytecode_only(";

fn ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Every `const NAME: &str = "value";` (or `&'static str`) of `text`, in
/// order, however the declaration is broken across lines.
fn string_consts(text: &str) -> Vec<(String, String)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(found) = text[i..].find("const ") {
        let at = i + found;
        i = at + "const ".len();
        if at > 0 && ident_byte(bytes[at - 1]) {
            continue;
        }
        let mut n = i;
        while n < bytes.len()
            && (bytes[n].is_ascii_uppercase() || bytes[n].is_ascii_digit() || bytes[n] == b'_')
        {
            n += 1;
        }
        if n == i || n >= bytes.len() || bytes[n] != b':' {
            continue;
        }
        let Some(semi) = text[n + 1..].find(';') else {
            continue;
        };
        let decl = &text[n + 1..n + 1 + semi];
        let Some((ty, val)) = decl.split_once('=') else {
            continue;
        };
        if !matches!(ty.trim(), "&str" | "&'static str") {
            continue;
        }
        let Some(inner) = val.trim().strip_prefix('"') else {
            continue;
        };
        let Some(end) = inner.find('"') else {
            continue;
        };
        out.push((text[i..n].to_string(), inner[..end].to_string()));
    }
    out
}

/// `name -> value` over every scanned file; `None` when two definitions
/// disagree (so a name is resolved across files only when it is unambiguous).
fn global_consts<'a>(texts: impl Iterator<Item = &'a str>) -> BTreeMap<String, Option<String>> {
    let mut out: BTreeMap<String, Option<String>> = BTreeMap::new();
    for text in texts {
        for (name, value) in string_consts(text) {
            let agrees = out
                .get(&name)
                .map(|have| have.as_deref() == Some(value.as_str()));
            match agrees {
                None => {
                    out.insert(name, Some(value));
                }
                Some(true) => {}
                Some(false) => {
                    out.insert(name, None);
                }
            }
        }
    }
    out
}

/// The binding of one call whose first argument is `arg`, in `text` (the
/// file), the call at byte `at`.
fn binding(
    text: &str,
    at: usize,
    arg: &str,
    local: &[(String, String)],
    global: &BTreeMap<String, Option<String>>,
) -> String {
    if arg.len() >= 2 && arg.starts_with('"') && arg.ends_with('"') {
        return format!("base {}", &arg[1..arg.len() - 1]);
    }
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        if let Some((_, value)) = local.iter().find(|(name, _)| name == arg) {
            return format!("base {value}");
        }
        if let Some(Some(value)) = global.get(arg) {
            return format!("base {value}");
        }
        return format!("base const {arg}");
    }
    let name = text[..at]
        .rfind("fn ")
        .map(|f| {
            let s = f + 3;
            let e = text[s..]
                .bytes()
                .position(|b| !ident_byte(b))
                .map_or(text.len(), |e| s + e);
            text[s..e].to_string()
        })
        .unwrap_or_default();
    format!("receiver in fn {name}")
}

/// Whether the end of `before` (a line's text up to a match) is inside a
/// string literal on that line: an odd number of unescaped `"`. A source scan
/// that quotes the call it looks for (`nio_file.rs`' own test of
/// `yield_to_foreign_path`) is not a call.
fn in_string_literal(before: &str) -> bool {
    let mut quotes = 0usize;
    let mut prev = 0u8;
    for b in before.bytes() {
        if b == b'"' && prev != b'\\' {
            quotes += 1;
        }
        prev = b;
    }
    quotes % 2 == 1
}

/// Whether the match at byte `at` of `text` is a call (not a comment, not a
/// quotation in a string literal, not the definition of a method of that
/// name).
fn is_call_site(text: &str, at: usize) -> bool {
    let line_start = text[..at].rfind('\n').map_or(0, |n| n + 1);
    let before = &text[line_start..at];
    !(before.contains("//") || before.trim_end().ends_with("fn") || in_string_literal(before))
}

/// The census of one file: `binding -> calls`.
fn census_of(
    text: &str,
    local: &[(String, String)],
    global: &BTreeMap<String, Option<String>>,
    into: &mut BTreeMap<String, usize>,
) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(found) = text[i..].find(NEEDLE) {
        let at = i + found;
        i = at + NEEDLE.len();
        if at > 0 && ident_byte(bytes[at - 1]) {
            continue;
        }
        let rest = &text[i..];
        let Some((suffix, variant)) = [
            ("(", ""),
            ("_by_class_id(", " (by_class_id)"),
            ("_hinted(", " (hinted)"),
        ]
        .into_iter()
        .find(|(suffix, _)| rest.starts_with(suffix)) else {
            continue;
        };
        i += suffix.len();
        if !is_call_site(text, at) {
            continue;
        }
        let mut j = i;
        while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\r' | b'\n') {
            j += 1;
        }
        let mut k = j;
        let mut depth = 0usize;
        while k < bytes.len() {
            match bytes[k] {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                b',' if depth == 0 => break,
                _ => {}
            }
            k += 1;
        }
        let arg = text[j..k].trim();
        *into
            .entry(format!(
                "{}{variant}",
                binding(text, at, arg, local, global)
            ))
            .or_default() += 1;
    }
}

/// The call sites of the receiver-class form in one file.
fn virtual_calls_of(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut calls = 0;
    while let Some(found) = text[i..].find(VIRTUAL_NEEDLE) {
        let at = i + found;
        i = at + VIRTUAL_NEEDLE.len();
        if at > 0 && ident_byte(bytes[at - 1]) {
            continue;
        }
        if is_call_site(text, at) {
            calls += 1;
        }
    }
    calls
}

/// `(label, text)` of every scanned source file, `<crate>/src/<path>`.
fn sources() -> Vec<(String, String)> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut out = Vec::new();
    for krate in CRATES {
        let base = repo.join(krate);
        let mut files = Vec::new();
        rust_files(&base.join("src"), &mut files);
        if REQUIRED_CRATES.contains(krate) {
            assert!(!files.is_empty(), "no sources under {}", base.display());
        }
        for path in files {
            let rel = path
                .strip_prefix(&base)
                .expect("under the crate")
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path).expect("a source file");
            out.push((format!("{krate}/{rel}"), text));
        }
    }
    out
}

fn census() -> BTreeMap<(String, String), usize> {
    let sources = sources();
    let global = global_consts(sources.iter().map(|(_, text)| text.as_str()));
    let mut out = BTreeMap::new();
    for (label, text) in &sources {
        let local = string_consts(text);
        let mut per_file = BTreeMap::new();
        census_of(text, &local, &global, &mut per_file);
        for (binding, calls) in per_file {
            out.insert((label.clone(), binding), calls);
        }
    }
    out
}

fn virtual_census() -> BTreeMap<String, usize> {
    sources()
        .into_iter()
        .filter_map(|(label, text)| {
            let calls = virtual_calls_of(&text);
            (calls > 0).then_some((label, calls))
        })
        .collect()
}

/// The reviewed verdict of a frozen row, `"open"` for a row not frozen yet.
fn verdict_of(file: &str, binding: &str) -> &'static str {
    FROZEN
        .iter()
        .find(|&&(f, b, _, _)| f == file && b == binding)
        .map_or("open", |&(_, _, _, v)| v)
}

fn kind_of(verdict: &str) -> &str {
    verdict.split_once(':').map_or(verdict, |(kind, _)| kind)
}

#[test]
fn router_census_is_frozen() {
    let now = census();
    let mut literal = String::from("const FROZEN: &[(&str, &str, usize, &str)] = &[\n");
    for ((file, binding), calls) in &now {
        literal.push_str(&format!(
            "    (\"{file}\", \"{binding}\", {calls}, \"{}\"),\n",
            verdict_of(file, binding)
        ));
    }
    literal.push_str("];\n");
    let total: usize = now.values().sum();
    let base = now
        .iter()
        .filter(|((_, b), _)| b.starts_with("base "))
        .map(|(_, n)| n)
        .sum::<usize>();
    println!(
        "router-census: {total} calls, {base} base-class, {} receiver",
        total - base
    );
    println!("{literal}");
    let frozen: BTreeMap<(String, String), usize> = FROZEN
        .iter()
        .map(|&(f, b, n, _)| ((f.to_string(), b.to_string()), n))
        .collect();
    assert_eq!(
        frozen.len(),
        FROZEN.len(),
        "FROZEN lists a (file, binding) twice"
    );
    if now != frozen {
        let mut diff = Vec::new();
        for (key, n) in &now {
            if frozen.get(key) != Some(n) {
                diff.push(format!("now {key:?} = {n}, frozen {:?}", frozen.get(key)));
            }
        }
        for (key, n) in &frozen {
            if !now.contains_key(key) {
                diff.push(format!("gone {key:?} (frozen {n})"));
            }
        }
        panic!(
            "the router census moved (see the module doc; re-freeze from the literal printed \
             above):\n{}",
            diff.join("\n")
        );
    }
}

/// Step 2 of the i74-L1 page, run over the whole census: no router is left
/// unreviewed, and a verdict that names a row names one that exists.
#[test]
fn every_router_row_has_a_reviewed_verdict() {
    for &(file, binding, _, verdict) in FROZEN {
        let kind = kind_of(verdict);
        assert!(
            KINDS.contains(&kind),
            "{file} {binding}: verdict {verdict:?} is not one of {KINDS:?}; a new router is \
             \"open\" until it is reviewed (the i74-L1 page, step 2)"
        );
        if matches!(kind, "family" | "covered") {
            let token = verdict
                .split_once(':')
                .map_or("", |(_, note)| note.split(' ').next().unwrap_or(""));
            assert!(
                Row::from_token(token).is_some(),
                "{file} {binding}: verdict {verdict:?} names no HotSpotAlignment token {token:?}"
            );
        }
    }
}

/// An unresolved `const` binding names nothing (the scan found no `&str`
/// definition, or two crates disagree): the row would hide its class.
#[test]
fn no_router_binding_is_an_unresolved_const() {
    for &(file, binding, _, _) in FROZEN {
        assert!(
            !binding.starts_with("base const "),
            "{file}: {binding}: the scan could not resolve the class name"
        );
    }
}

/// The census as a report, per class (or per computing function) and per
/// verdict kind: `cargo test -p cratonvm-native-builtins --test router_census
/// router_census_report -- --nocapture`.
#[test]
fn router_census_report_by_class() {
    let mut by_class: BTreeMap<String, (usize, BTreeSet<String>)> = BTreeMap::new();
    let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
    for &(_, binding, calls, verdict) in FROZEN {
        let entry = by_class.entry(binding.to_string()).or_default();
        entry.0 += calls;
        entry.1.insert(verdict.to_string());
        *by_kind.entry(kind_of(verdict)).or_default() += calls;
    }
    println!(
        "router-census report: {} classes / functions",
        by_class.len()
    );
    for (class, (calls, verdicts)) in &by_class {
        println!(
            "  {calls:3}  {class}  =>  {}",
            verdicts.iter().cloned().collect::<Vec<_>>().join(" | ")
        );
    }
    println!("router-census report by verdict kind: {by_kind:?}");
    let total: usize = FROZEN.iter().map(|&(_, _, n, _)| n).sum();
    assert_eq!(by_kind.values().sum::<usize>(), total);
}

#[test]
fn virtual_router_census_is_frozen() {
    let now = virtual_census();
    let mut literal = String::from("const VIRTUAL_FROZEN: &[(&str, usize)] = &[\n");
    for (file, calls) in &now {
        literal.push_str(&format!("    (\"{file}\", {calls}),\n"));
    }
    literal.push_str("];\n");
    println!(
        "virtual-router-census: {} sites in {} files",
        now.values().sum::<usize>(),
        now.len()
    );
    println!("{literal}");
    let frozen: BTreeMap<String, usize> = VIRTUAL_FROZEN
        .iter()
        .map(|&(f, n)| (f.to_string(), n))
        .collect();
    if now != frozen {
        let mut diff = Vec::new();
        for (key, n) in &now {
            if frozen.get(key) != Some(n) {
                diff.push(format!("now {key:?} = {n}, frozen {:?}", frozen.get(key)));
            }
        }
        for (key, n) in &frozen {
            if !now.contains_key(key) {
                diff.push(format!("gone {key:?} (frozen {n})"));
            }
        }
        panic!(
            "the receiver-class router census moved: a new `invoke_virtual_bytecode_only` \
             native re-dispatches on the receiver, so a subclass override that calls \
             `super.m()` bounces back into it (see `delegate_to_real_bytecode` and \
             i74x-L1-receiver-class-routers-super-bounce-audit-20261102.md); \
             review it, then re-freeze from the literal printed above:\n{}",
            diff.join("\n")
        );
    }
}

#[test]
fn the_scan_classifies_the_three_bindings() {
    let text = "const ZIP: &str = \"java/util/zip/ZipUtils\";\n\
        fn a(ctx: &mut dyn C) { ctx.invoke_special_bytecode_only(\"java/net/URL\", \"m\", \"()V\", &[]); }\n\
        fn b(ctx: &mut dyn C) { ctx.invoke_special_bytecode_only(ZIP, \"m\", \"()V\", &[]); }\n\
        fn route_it(ctx: &mut dyn C, cls: &str) { ctx.invoke_special_bytecode_only_hinted(\n    cls, \"m\", \"()V\", &[]); }\n\
        // ctx.invoke_special_bytecode_only(\"commented/Out\", ..)\n\
        fn invoke_special_bytecode_only(&mut self) {}\n";
    let mut got = BTreeMap::new();
    census_of(text, &string_consts(text), &BTreeMap::new(), &mut got);
    let got: Vec<(String, usize)> = got.into_iter().collect();
    assert_eq!(
        got,
        vec![
            ("base java/net/URL".to_string(), 1),
            ("base java/util/zip/ZipUtils".to_string(), 1),
            ("receiver in fn route_it (hinted)".to_string(), 1),
        ]
    );
}

#[test]
fn consts_resolve_across_lines_and_files_and_only_when_unambiguous() {
    let here = "const LONG: &str =\n    \"a/B$C\";\n\
        fn f(ctx: &mut dyn C) {\n\
            ctx.invoke_special_bytecode_only(LONG, \"m\", \"()V\", &[]);\n\
            ctx.invoke_special_bytecode_only(ELSEWHERE, \"m\", \"()V\", &[]);\n\
            ctx.invoke_special_bytecode_only(CLASHES, \"m\", \"()V\", &[]);\n\
            ctx.invoke_special_bytecode_only(UNKNOWN, \"m\", \"()V\", &[]);\n\
        }\n";
    let there =
        "pub(crate) const ELSEWHERE: &'static str = \"x/Y\";\nconst CLASHES: &str = \"p/Q\";\n\
        const NOT_A_STR: usize = 4;\nconst fn helper() {}\n";
    let other = "const CLASHES: &str = \"p/R\";\n";
    let global = global_consts([here, there, other].into_iter());
    assert_eq!(global.get("ELSEWHERE"), Some(&Some("x/Y".to_string())));
    assert_eq!(global.get("CLASHES"), Some(&None));
    assert_eq!(global.get("NOT_A_STR"), None);
    let mut got = BTreeMap::new();
    census_of(here, &string_consts(here), &global, &mut got);
    let got: Vec<(String, usize)> = got.into_iter().collect();
    assert_eq!(
        got,
        vec![
            ("base a/B$C".to_string(), 1),
            ("base const CLASHES".to_string(), 1),
            ("base const UNKNOWN".to_string(), 1),
            ("base x/Y".to_string(), 1),
        ]
    );
}

#[test]
fn the_virtual_scan_counts_calls_only() {
    let text = "fn a(ctx: &mut dyn C) { ctx.invoke_virtual_bytecode_only(this, \"m\", \"()V\", &[]); }\n\
        // ctx.invoke_virtual_bytecode_only(this, \"commented\", \"()V\", &[])\n\
        fn invoke_virtual_bytecode_only(&mut self) {}\n\
        fn b(ctx: &mut dyn C) {\n    scope.invoke_virtual_bytecode_only(\n        this, \"n\", \"()V\", &[]);\n}\n\
        fn my_invoke_virtual_bytecode_only(x: i32) { let _ = x; }\n\
        let q = body.find(\"ctx.invoke_virtual_bytecode_only(\");\n\
        let r = format!(\"{}\", ctx.invoke_virtual_bytecode_only(this, \"o\", \"()V\", &[]));\n";
    // two calls, the quoted `find` argument is not one, the `format!` argument is
    assert_eq!(virtual_calls_of(text), 3);
}
