// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The router census (interpreter round i6 wave 118, lane L1; step 1 of
//! `docs/feature-designs/native-dispatch-architecture.md`).
//!
//! A "router" is a registered native that, for a real receiver, does nothing
//! but run the JDK's (or the library's) own bytecode again through
//! `NativeContext::invoke_special_bytecode_only` (or its `_by_class_id` /
//! `_hinted` forms). In `--compatible` it is a native door plus a second
//! dispatch per call; in `--jdk-only` the bytecode wins anyway. Waves 74-82
//! turned the whole-class j.u.c ones into family rows; this census says
//! where the rest are, per file, per class and per BINDING RULE, which is
//! Java-visible (wave 75, `i75-L1-jdk-only-copyonwritearrayset-subclass-super-add-loses-the-element`):
//!
//! * `base <class>`: the router names the class whose body it runs (a
//!   literal, or a `const` of the same file): a subclass's `super.m()` runs
//!   that class's body, as `invokespecial` does;
//! * `receiver in fn <f>`: the class is computed at run time (the receiver's
//!   class, a descriptor's owner): the router in function `f` binds on what it
//!   is handed, and a subclass's bridge method can re-enter it.
//!
//! The census is a SOURCE scan (host- and feature-independent): every call of
//! the three methods in `native-builtins/src` and `native-collections/src`,
//! outside `//` comments. [`FROZEN`] holds it with zero slack, so a new router
//! (or a router turned into a family row) is a named line in the diff, not a
//! grep a later wave repeats by hand. Re-freeze by pasting the printed
//! literal (`cargo test -p cratonvm-native-builtins --test router_census --
//! --nocapture`), and say on the i74-L1 page why the row moved.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `(file, binding, calls)`, sorted: the census at the wave-118 freeze.
const FROZEN: &[(&str, &str, usize)] = &[
    ("native-builtins/src/antlr_intrinsics.rs", "receiver in fn antlr_null_case_runs_bytecode", 1),
    ("native-builtins/src/apps_h2.rs", "base org/h2/expression/ExpressionColumn", 1),
    ("native-builtins/src/apps_h2.rs", "receiver in fn h2_session_prepare_local_no_cache (by_class_id)", 1),
    ("native-builtins/src/charset.rs", "base java/lang/String", 1),
    ("native-builtins/src/classloader.rs", "base java/lang/ClassLoader", 4),
    ("native-builtins/src/classloader.rs", "base jdk/internal/loader/Resource", 1),
    ("native-builtins/src/classloader.rs", "base jdk/internal/loader/URLClassPath$Loader", 1),
    // zt t31 merge: dev's i7 w135 hinted `addClass` upcall (opt-in), the loader's own
    // `addClass` body.
    ("native-builtins/src/classloader.rs", "receiver in fn call_loader_add_class_hinted (hinted)", 1),
    // zt define-path rounds: the jdk-only built-in loader rows run
    // `BuiltinClassLoader`'s own bytecode first (see the i74-L1 page).
    ("native-builtins/src/classloader_real.rs", "base jdk/internal/loader/BuiltinClassLoader", 2),
    ("native-builtins/src/http_url_connection.rs", "receiver in fn https_forward_to_super", 1),
    ("native-builtins/src/jca/message_digest.rs", "base java/security/MessageDigest", 1),
    ("native-builtins/src/jdbc.rs", "base org/apache/derby/iapi/jdbc/InternalDriver", 1),
    ("native-builtins/src/jmx.rs", "base java/lang/management/MemoryUsage", 1),
    ("native-builtins/src/jmx.rs", "base sun/management/GarbageCollectorImpl", 1),
    ("native-builtins/src/jmx.rs", "base sun/management/MemoryManagerImpl", 1),
    ("native-builtins/src/jmx.rs", "base sun/management/MemoryPoolImpl", 1),
    ("native-builtins/src/lang_class.rs", "base java/lang/ClassLoader", 4),
    ("native-builtins/src/lang_class.rs", "receiver in fn annotated_type_real_body", 1),
    // Interpreter round i7 wave 137, lane L4 (`CRATONVM_MH_LF_LEAF_INTRINSICS`):
    // `internalMemberName` / `checkSpreadArgument`'s own bodies.
    ("native-builtins/src/lang_invoke.rs", "base java/lang/invoke/DirectMethodHandle", 1),
    ("native-builtins/src/lang_invoke.rs", "base java/lang/invoke/InvokerBytecodeGenerator", 1),
    ("native-builtins/src/lang_invoke.rs", "base java/lang/invoke/Invokers", 1),
    ("native-builtins/src/lang_invoke.rs", "base java/lang/invoke/LambdaMetafactory", 1),
    ("native-builtins/src/lang_invoke.rs", "base java/lang/invoke/MemberName", 1),
    ("native-builtins/src/lang_invoke.rs", "base java/lang/invoke/MethodHandle", 1),
    ("native-builtins/src/lang_invoke.rs", "base java/lang/invoke/MethodHandleImpl", 1),
    ("native-builtins/src/lang_invoke.rs", "base java/lang/invoke/MethodHandles", 1),
    ("native-builtins/src/lang_invoke.rs", "base java/lang/invoke/MethodType", 1),
    ("native-builtins/src/lang_invoke.rs", "base java/lang/invoke/VarHandle", 4),
    ("native-builtins/src/lang_invoke.rs", "receiver in fn mt_make_impl_jdk_body (hinted)", 1),
    ("native-builtins/src/lang_math.rs", "base java/lang/Math", 1),
    ("native-builtins/src/lang_math.rs", "receiver in fn number_parse_failed", 1),
    ("native-builtins/src/lang_misc.rs", "receiver in fn null_arg_runs_shadowed_bytecode", 1),
    ("native-builtins/src/lang_misc.rs", "receiver in fn shadow_refusal_runs_bytecode", 1),
    ("native-builtins/src/lang_reflect.rs", "base sun/reflect/generics/reflectiveObjects/TypeVariableImpl", 1),
    ("native-builtins/src/lang_stackwalker.rs", "base java/lang/StackStreamFactory$StackFrameBuffer", 1),
    // 2 -> 3, JIT round 23 wave 2, lane L6: `string_error_from_jdk_body`
    // (`CRATONVM_STRING_ERRORS_FROM_JDK_BODY`) runs `String`'s own
    // codePointAt / codePointCount / offsetByCodePoints body on the FAILING
    // path only, so the exception carries the JDK frames; not a per-call
    // router (see the i74-L1 page).
    ("native-builtins/src/lang_string.rs", "base java/lang/String", 3),
    ("native-builtins/src/lang_string.rs", "receiver in fn sb_append_object_through_bytecode", 1),
    ("native-builtins/src/lang_string.rs", "receiver in fn sb_raise_through_builder_body", 1),
    ("native-builtins/src/lib.rs", "receiver in fn delegate_to_real_bytecode", 1),
    ("native-builtins/src/locale_resources.rs", "base java/util/ResourceBundle", 1),
    ("native-builtins/src/logging_shims.rs", "base org/slf4j/impl/StaticLoggerBinder", 1),
    ("native-builtins/src/logging_shims.rs", "base org/slf4j/impl/StaticMDCBinder", 1),
    ("native-builtins/src/lucene_es.rs", "base java/util/concurrent/AbstractExecutorService", 2),
    ("native-builtins/src/lucene_es.rs", "base java/util/concurrent/ThreadPoolExecutor", 1),
    ("native-builtins/src/net_phase_e.rs", "base java/net/JarURLConnection", 1),
    ("native-builtins/src/net_phase_e.rs", "base java/net/URL", 3),
    ("native-builtins/src/net_phase_e.rs", "base java/net/URLConnection", 2),
    ("native-builtins/src/net_phase_e.rs", "base org/springframework/boot/web/reactive/context/ReactiveWebServerApplicationContext", 1),
    ("native-builtins/src/net_phase_e.rs", "base org/springframework/boot/web/server/servlet/context/ServletWebServerApplicationContext", 1),
    ("native-builtins/src/net_phase_e.rs", "base org/springframework/boot/web/servlet/context/ServletWebServerApplicationContext", 1),
    ("native-builtins/src/net_phase_e.rs", "base org/springframework/data/redis/core/RedisAccessor", 1),
    ("native-builtins/src/net_phase_e.rs", "base org/springframework/data/redis/listener/RedisMessageListenerContainer", 1),
    ("native-builtins/src/net_phase_e.rs", "base org/springframework/session/data/redis/RedisOperationsSessionRepository", 2),
    ("native-builtins/src/net_phase_e.rs", "base org/springframework/session/data/redis/config/annotation/web/http/RedisHttpSessionConfiguration$EnableRedisKeyspaceNotificationsInitializer", 1),
    ("native-builtins/src/net_phase_e.rs", "base org/springframework/util/ClassUtils", 1),
    ("native-builtins/src/net_phase_e.rs", "base org/springframework/web/context/support/WebApplicationContextUtils$FacesDependencyRegistrar", 1),
    ("native-builtins/src/orm_hibernate.rs", "receiver in fn hibernate_null_required_arg_runs_bytecode", 1),
    ("native-builtins/src/orm_hibernate.rs", "receiver in fn hibernate_null_required_arg_runs_bytecode (by_class_id)", 1),
    ("native-builtins/src/panama.rs", "base jdk/internal/foreign/AbstractMemorySegmentImpl", 1),
    ("native-builtins/src/phases_early.rs", "base java/lang/StringLatin1", 1),
    ("native-builtins/src/phases_early.rs", "base java/lang/ThreadLocal", 3),
    ("native-builtins/src/phases_early.rs", "base jdk/internal/util/ArraysSupport", 1),
    ("native-builtins/src/phases_late/bouncycastle.rs", "base org/bouncycastle/crypto/modes/CBCBlockCipher", 1),
    ("native-builtins/src/phases_late/bouncycastle.rs", "base org/springframework/security/crypto/bcrypt/BCrypt", 1),
    ("native-builtins/src/phases_late/bouncycastle.rs", "receiver in fn bc_aes_native_ctor", 1),
    ("native-builtins/src/phases_late/concurrent.rs", "base java/lang/Thread", 2),
    ("native-builtins/src/phases_late/concurrent.rs", "base java/util/concurrent/ThreadPoolExecutor", 1),
    ("native-builtins/src/phases_late/foreign_ffm.rs", "base jdk/internal/foreign/layout/AbstractLayout", 2),
    ("native-builtins/src/phases_late/jar_manifest.rs", "base org/springframework/boot/loader/launch/JarFileArchive", 1),
    ("native-builtins/src/phases_late/nio_file.rs", "base java/nio/file/Path", 1),
    ("native-builtins/src/phases_late/nio_file.rs", "base java/nio/file/attribute/PosixFilePermissions", 1),
    ("native-builtins/src/phases_late/nio_file.rs", "receiver in fn yield_to_foreign_path", 2),
    ("native-builtins/src/phases_late/zip_streams.rs", "base java/util/zip/Deflater", 1),
    ("native-builtins/src/phases_late/zip_streams.rs", "base java/util/zip/GZIPInputStream", 1),
    ("native-builtins/src/phases_late/zip_streams.rs", "base java/util/zip/GZIPOutputStream", 1),
    ("native-builtins/src/phases_late/zip_streams.rs", "base java/util/zip/Inflater", 1),
    ("native-builtins/src/preconditions.rs", "receiver in fn raise_through_bytecode", 1),
    ("native-builtins/src/regex_matcher.rs", "base java/util/regex/Matcher", 1),
    ("native-builtins/src/scoped_twins.rs", "base jdk/internal/misc/ScopedMemoryAccess", 1),
    // dev `61f85ed8a2` (merged into JIT round 23): `SecureRandom.getInstance`
    // defers to the JDK body for application-installed providers (BouncyCastle
    // JSSE), a base-class call on the provider-lookup path, not a per-call router.
    ("native-builtins/src/securerandom.rs", "base java/security/SecureRandom", 1),
    ("native-builtins/src/securerandom.rs", "base java/util/Random", 1),
    ("native-builtins/src/util_concurrent_ext.rs", "base java/util/concurrent/AbstractExecutorService", 2),
    ("native-builtins/src/util_concurrent_ext.rs", "base java/util/concurrent/ThreadPoolExecutor", 3),
    ("native-builtins/src/util_concurrent_ext.rs", "base java/util/concurrent/locks/ReentrantLock", 1),
    ("native-builtins/src/util_concurrent_ext.rs", "receiver in fn rl_fast_jdk_body (hinted)", 1),
    ("native-builtins/src/vector_support_intrinsics.rs", "receiver in fn template_fallback", 1),
    ("native-builtins/src/zip_real.rs", "base java/util/zip/ZipCoder$UTF8ZipCoder", 1),
    ("native-builtins/src/zip_real.rs", "base java/util/zip/ZipFile$Source", 2),
    ("native-builtins/src/zip_real.rs", "base java/util/zip/ZipUtils", 3),
    ("native-collections/src/lib.rs", "base java/util/ArrayDeque", 1),
    ("native-collections/src/lib.rs", "base java/util/HashMap", 3),
    ("native-collections/src/lib.rs", "base java/util/HashMap$TreeNode", 7),
    ("native-collections/src/lib.rs", "base java/util/concurrent/CompletableFuture", 1),
    ("native-collections/src/lib.rs", "base java/util/concurrent/CopyOnWriteArraySet", 1),
    ("native-collections/src/lib.rs", "base java/util/concurrent/ThreadPoolExecutor", 1),
    ("native-collections/src/lib.rs", "receiver in fn chm_door_runs_jdk_body", 1),
    ("native-collections/src/lib.rs", "receiver in fn try_delegate_real_collection_args", 1),
];

const NEEDLE: &str = "invoke_special_bytecode_only";

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

/// The binding of one call whose first argument is `arg`, in `text` (the
/// file), the call at byte `at`.
fn binding(text: &str, at: usize, arg: &str) -> String {
    if arg.len() >= 2 && arg.starts_with('"') && arg.ends_with('"') {
        return format!("base {}", &arg[1..arg.len() - 1]);
    }
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        for marker in [
            format!("const {arg}: &str = \""),
            format!("const {arg}: &'static str = \""),
        ] {
            if let Some(m) = text.find(&marker) {
                let s = m + marker.len();
                let e = text[s..].find('"').map_or(text.len(), |e| s + e);
                return format!("base {}", &text[s..e]);
            }
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

/// The census of one file: `binding -> calls`.
fn census_of(text: &str, into: &mut BTreeMap<String, usize>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(found) = text[i..].find(NEEDLE) {
        let at = i + found;
        i = at + NEEDLE.len();
        if at > 0 && ident_byte(bytes[at - 1]) {
            continue;
        }
        let rest = &text[i..];
        let Some((suffix, variant)) = [("(", ""), ("_by_class_id(", " (by_class_id)"), ("_hinted(", " (hinted)")]
            .into_iter()
            .find(|(suffix, _)| rest.starts_with(suffix))
        else {
            continue;
        };
        i += suffix.len();
        let line_start = text[..at].rfind('\n').map_or(0, |n| n + 1);
        let before = &text[line_start..at];
        if before.contains("//") || before.trim_end().ends_with("fn") {
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
        *into.entry(format!("{}{variant}", binding(text, at, arg))).or_default() += 1;
    }
}

fn census() -> BTreeMap<(String, String), usize> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut out = BTreeMap::new();
    for krate in ["native-builtins", "native-collections"] {
        let base = repo.join(krate);
        let mut files = Vec::new();
        rust_files(&base.join("src"), &mut files);
        assert!(!files.is_empty(), "no sources under {}", base.display());
        for path in files {
            let rel = path
                .strip_prefix(&base)
                .expect("under the crate")
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path).expect("a source file");
            let mut per_file = BTreeMap::new();
            census_of(&text, &mut per_file);
            for (binding, calls) in per_file {
                out.insert((format!("{krate}/{rel}"), binding), calls);
            }
        }
    }
    out
}

#[test]
fn router_census_is_frozen() {
    let now = census();
    let mut literal = String::from("const FROZEN: &[(&str, &str, usize)] = &[\n");
    for ((file, binding), calls) in &now {
        literal.push_str(&format!("    (\"{file}\", \"{binding}\", {calls}),\n"));
    }
    literal.push_str("];\n");
    let total: usize = now.values().sum();
    let base = now.iter().filter(|((_, b), _)| b.starts_with("base ")).map(|(_, n)| n).sum::<usize>();
    println!("router-census: {total} calls, {base} base-class, {} receiver", total - base);
    println!("{literal}");
    let frozen: BTreeMap<(String, String), usize> = FROZEN
        .iter()
        .map(|&(f, b, n)| ((f.to_string(), b.to_string()), n))
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
            "the router census moved (see the module doc; re-freeze from the literal printed \
             above):\n{}",
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
    census_of(text, &mut got);
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
