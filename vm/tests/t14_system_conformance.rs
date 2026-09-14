// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T14 — `java/lang/System` bootstrap chain conformance test suite.
//!
//! Verifies that all T14 System and VM native methods are registered,
//! the bootstrap chain (initPhase1/2/3) is wired, and no stubs remain.
//!
//!     cargo test -p cratonvm-vm --test t14_system_conformance -- --nocapture

use std::path::{Path, PathBuf};

/// Returns the workspace root (parent of the `vm` crate).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
}

/// Read a workspace-relative source file.
fn read_ws(rel: &str) -> String {
    let path = workspace_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Read a module: the `foo.rs` file plus every `foo/*.rs` submodule.
///
/// A check that names a single file silently stops covering the code when that
/// file is split — and `vm/src/runtime/interpreter.rs` has been split twice now.
/// `t11_safety_conformance` already reads this module the same way; this is the
/// same fix applied to the one T14 check that did not.
fn read_module(rel: &str) -> String {
    let mut src = read_ws(rel);
    let dir = workspace_root().join(rel.trim_end_matches(".rs"));
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut paths: Vec<_> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("rs"))
            .collect();
        paths.sort();
        for p in paths {
            src.push('\n');
            src.push_str(&std::fs::read_to_string(&p).unwrap_or_default());
        }
    }
    src
}

/// Whitespace-stripped copy of a source file. `registry.register(...)` calls
/// are frequently wrapped across several lines by rustfmt, so a per-line text
/// scan misses them (and conflicts with `cargo fmt`). Matching against the
/// whitespace-free form makes the `("class", "method", "descriptor")` tuple
/// detectable regardless of line wrapping.
fn compact_ws(src: &str) -> String {
    src.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The registration forms this scanner recognises.
///
/// `register_with_kind` is not a variant spelling — it is how 230 of the
/// registrations in `native-builtins/src/lib.rs` are written, against
/// 1207 plain ones. Recognising only `register(` is why this gate went
/// red claiming `java/lang/System::setIn0` was missing: it is registered,
/// eleven thousand lines into that file, with a `NativeKind` attached.
///
/// A scanner that reports a live registration as missing is worse than
/// one that is merely wrong, because the obvious response is to go and
/// "add" a native that is already there.
const REGISTER_FORMS: &[&str] = &["register(", "register_with_kind("];

/// Byte offsets of every registration **call site** for this triple in the
/// whitespace-free source. Each offset points at the `r` of `register`.
///
/// The `register` prefix is load-bearing: `native-builtins/src/lib.rs` also
/// contains unit tests that assert on the same triples via
/// `registry.find("class", "method", "descriptor")`, and matching the bare
/// tuple would let one of those satisfy `registers()` even after the real
/// registration was deleted. Both forms below keep that property — neither
/// `find(` nor anything else that takes the same triple ends in
/// `register(` or `register_with_kind(`.
fn register_sites(compact: &str, class: &str, method: &str, desc: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for form in REGISTER_FORMS {
        let needle = format!("{form}\"{class}\",\"{method}\",\"{desc}\"");
        let mut from = 0usize;
        while let Some(rel) = compact[from..].find(&needle) {
            out.push(from + rel);
            from += rel + 1;
        }
    }
    out.sort_unstable();
    out
}

/// True if the (whitespace-free) source contains a
/// `register("class", "method", "descriptor", ...)` call in any wrapping.
fn registers(compact: &str, class: &str, method: &str, desc: &str) -> bool {
    !register_sites(compact, class, method, desc).is_empty()
}

/// True if the `register(` call site at `site` carries a `#[cfg(...)]`
/// attribute — i.e. it can be compiled out of some build configuration.
///
/// Anchored on the call itself rather than "is there a `#[cfg(` anywhere
/// earlier in the statement": walk back over the receiver (`registry.`), then
/// peel trailing `#[...]` attribute groups. Scanning the whole preceding
/// statement would false-positive on any *comment* that quotes a `#[cfg(...)]`
/// line — and the comment above `latestUserDefinedLoader0`, the native this
/// check exists for, does exactly that.
fn register_site_is_cfg_gated(compact: &str, site: usize) -> bool {
    let head = &compact[..site];
    let bytes = head.as_bytes();
    let mut i = head.len();
    if i > 0 && bytes[i - 1] == b'.' {
        i -= 1; // the `.` of `registry.register(`
        while i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
            i -= 1;
        }
    }
    let mut tail = &head[..i];
    while tail.ends_with(']') {
        let Some(open) = tail.rfind("#[") else { break };
        if tail[open..].starts_with("#[cfg(") {
            return true;
        }
        tail = &tail[..open];
    }
    false
}

/// The ~240 chars following a `"class","method"` registration key in the
/// whitespace-free source — i.e. the descriptor + handler argument, used to
/// assert a registration is wired to a real impl rather than a stub closure.
fn reg_args<'a>(compact: &'a str, class: &str, method: &str) -> Option<&'a str> {
    let key = format!("\"{class}\",\"{method}\"");
    let i = compact.find(&key)?;
    let start = i + key.len();
    let end = (start + 240).min(compact.len());
    Some(&compact[start..end])
}

/// The handler argument of a specific `register("class","method","desc", H)`
/// call: the text from just after the (class, method, descriptor) tuple up to
/// the call-terminating `;`. Bounded to the single call so it cannot spill into
/// an adjacent registration (e.g. a neighbouring stub returning `Object(None)`).
fn reg_handler<'a>(compact: &'a str, class: &str, method: &str, desc: &str) -> Option<&'a str> {
    let key = format!("\"{class}\",\"{method}\",\"{desc}\"");
    let i = compact.find(&key)?;
    let rest = &compact[i + key.len()..];
    let end = rest.find(';').map(|e| e + 1).unwrap_or(rest.len());
    Some(&rest[..end])
}

// ===========================================================================
// T14.1 — Phase 1 bootstrap methods registered
// ===========================================================================

/// All java/lang/System native methods that must be registered.
const SYSTEM_NATIVES: &[(&str, &str)] = &[
    ("registerNatives", "()V"),
    ("currentTimeMillis", "()J"),
    ("nanoTime", "()J"),
    ("arraycopy", "(Ljava/lang/Object;ILjava/lang/Object;II)V"),
    ("identityHashCode", "(Ljava/lang/Object;)I"),
    ("exit", "(I)V"),
    ("setIn0", "(Ljava/io/InputStream;)V"),
    ("setOut0", "(Ljava/io/PrintStream;)V"),
    ("setErr0", "(Ljava/io/PrintStream;)V"),
    ("mapLibraryName", "(Ljava/lang/String;)Ljava/lang/String;"),
    // T14 bootstrap chain
    ("initPhase1", "()V"),
    ("initPhase2", "(ZZ)I"),
    ("initPhase3", "()V"),
];

#[test]
fn t14_all_system_natives_registered() {
    let compact = compact_ws(&read_ws("native-builtins/src/lib.rs"));
    let mut missing: Vec<String> = Vec::new();

    for &(method, descriptor) in SYSTEM_NATIVES {
        if !registers(&compact, "java/lang/System", method, descriptor) {
            missing.push(format!("{method}{descriptor}"));
        }
    }

    assert!(
        missing.is_empty(),
        "T14: {}/{} System natives missing:\n  {}",
        missing.len(),
        SYSTEM_NATIVES.len(),
        missing.join("\n  "),
    );
    eprintln!(
        "[T14.1] ✓ All {}/{} java/lang/System natives registered",
        SYSTEM_NATIVES.len(),
        SYSTEM_NATIVES.len(),
    );
}

// ===========================================================================
// T14.2 — initPhase1/2/3 point to real implementations
// ===========================================================================

#[test]
fn t14_init_phases_use_named_functions() {
    let lib = read_ws("native-builtins/src/lib.rs");

    let compact = compact_ws(&lib);
    let patterns = [
        ("initPhase1", "native_system_init_phase1"),
        ("initPhase2", "native_system_init_phase2"),
        ("initPhase3", "native_system_init_phase3"),
    ];

    for (method, func) in &patterns {
        let found =
            reg_args(&compact, "java/lang/System", method).is_some_and(|w| w.contains(func));
        assert!(
            found,
            "T14: initPhase method '{}' not wired to '{}'",
            method, func,
        );
    }
    eprintln!("[T14.2] ✓ initPhase1/2/3 all point to named implementations");
}

// ===========================================================================
// T14.3 — VM info natives registered
// ===========================================================================

const VM_NATIVES: &[(&str, &str)] = &[
    ("initialize", "()V"),
    ("initLevel", "()I"),
    ("getSavedProperty", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("getNanoTimeAdjustment", "(J)J"),
    ("getRuntimeArguments", "()[Ljava/lang/String;"),
    ("latestUserDefinedLoader0", "()Ljava/lang/ClassLoader;"),
    ("getuid", "()J"),
    ("geteuid", "()J"),
    ("getgid", "()J"),
    ("getegid", "()J"),
];

#[test]
fn t14_all_vm_natives_registered() {
    let compact = compact_ws(&read_ws("native-builtins/src/lib.rs"));
    let mut missing: Vec<String> = Vec::new();

    for &(method, descriptor) in VM_NATIVES {
        if !registers(&compact, "jdk/internal/misc/VM", method, descriptor) {
            missing.push(format!("{method}{descriptor}"));
        }
    }

    assert!(
        missing.is_empty(),
        "T14: {}/{} VM natives missing:\n  {}",
        missing.len(),
        VM_NATIVES.len(),
        missing.join("\n  "),
    );
    eprintln!(
        "[T14.3] ✓ All {}/{} jdk/internal/misc/VM natives registered",
        VM_NATIVES.len(),
        VM_NATIVES.len(),
    );
}

// ===========================================================================
// T14.3b — no VM native is compiled out of a default build
// ===========================================================================

/// `t14_all_vm_natives_registered` is a source-text scan, so it reports a
/// registration as present even when a `#[cfg(feature = "...")]` attribute
/// compiles it out. That false green is exactly how
/// `jdk/internal/misc/VM.latestUserDefinedLoader0()` shipped unregistered for
/// weeks: it sat behind `#[cfg(any(feature = "experimental-serialization",
/// feature = "synthetic-jdk"))]`, neither of which is a default feature of
/// `cratonvm-vm` or `cratonvm-cli`, so every plain
/// `cargo build --release -p cratonvm-cli` threw `UnsatisfiedLinkError` on
/// each `ObjectInputStream.readObject()` of an ordinary class.
///
/// This test closes the gap for the whole `VM_NATIVES` list: a triple whose
/// every `register(` call site carries a `#[cfg(...)]` is a failure, whatever
/// the condition. If some future VM native genuinely must be optional, add it
/// to an explicit allow-list here with a written reason rather than deleting
/// the check.
#[test]
fn t14_vm_natives_not_feature_gated() {
    let compact = compact_ws(&read_ws("native-builtins/src/lib.rs"));
    let mut gated: Vec<String> = Vec::new();

    for &(method, descriptor) in VM_NATIVES {
        let sites = register_sites(&compact, "jdk/internal/misc/VM", method, descriptor);
        if sites.is_empty() {
            continue; // absence is t14_all_vm_natives_registered's job
        }
        if !sites
            .iter()
            .any(|&site| !register_site_is_cfg_gated(&compact, site))
        {
            gated.push(format!("{method}{descriptor}"));
        }
    }

    assert!(
        gated.is_empty(),
        "T14: {} VM native registration(s) are behind a #[cfg(...)] and so are \
         absent from a default build:\n  {}",
        gated.len(),
        gated.join("\n  "),
    );
    eprintln!("[T14.3b] \u{2713} No jdk/internal/misc/VM native is feature-gated");
}

// ===========================================================================
// T14.3c — every #[cfg]-gated native registration is on a declared allow-list
// ===========================================================================

/// `t14_vm_natives_not_feature_gated` above closes the hole for the
/// `VM_NATIVES` list. This closes it for **every** native registration in
/// `native-builtins/src/lib.rs`, because the mechanism that produced the
/// `latestUserDefinedLoader0` bug was never specific to `jdk/internal/misc/VM`.
///
/// The mechanism: `libcratonvm` depends on `cratonvm-vm` with
/// `experimental-serialization`/`-aot`/`-debug` enabled, and Cargo unifies
/// features across every member built in one invocation. So
/// `cargo build/test --workspace` (CI) resolves those features ON for
/// `cratonvm-native-builtins`, while `cargo build --release -p cratonvm-cli`
/// (what every suite runner uses) resolves them OFF. Any
/// `#[cfg(feature = ...)]` on a registration is therefore a silent behaviour
/// fork between the binary CI tests and the binary every suite runs — and both
/// sides look green, because neither can see the other's configuration.
///
/// This test does not forbid gating. Some natives genuinely must be
/// synthetic-only: in synthetic-JDK mode there is no JDK bytecode to fall back
/// to, so the Rust implementation *is* the implementation. It forbids
/// **undeclared** gating. Every gated registration is listed below with the
/// reason it may differ between builds; a new one fails this test until
/// somebody writes down why.
///
/// Two real gates were removed while this was written, both of which claimed a
/// real-JDK-mode role in their own comments while being compiled out of every
/// real-JDK-mode suite build: `jdk/internal/misc/VM.latestUserDefinedLoader0`
/// (an `UnsatisfiedLinkError` on every `ObjectInputStream.readObject()`) and
/// `serialization::register_reflection_factory_serialization`.
#[test]
fn t14_gated_registrations_are_declared() {
    /// A `#[cfg]`-gated registration that is allowed to be absent from a
    /// default build. `cfg` and `target` are matched against the trimmed
    /// source lines exactly; `reason` is documentation for the next reader.
    struct Allowed {
        cfg: &'static str,
        target: &'static str,
        #[allow(dead_code)]
        reason: &'static str,
    }

    let allowed = [
        Allowed {
            cfg: "#[cfg(feature = \"synthetic-jdk\")]",
            target: "registry.register(",
            reason: "java/lang/System.initPhase1/2/3 — synthetic-jdk only by \
                     design. In real-JDK mode the JDK's own bytecode runs the \
                     bootstrap, supported by the T14 SystemProps$Raw / \
                     FileDescriptor natives.",
        },
        Allowed {
            cfg: "#[cfg(feature = \"synthetic-jdk\")]",
            target: "crate::wildfly_naming::register_jdk_naming_natives(registry);",
            reason: "Synthetic JNDI. Must NOT be registered in real-JDK mode, \
                     where it would shadow real provider selection.",
        },
        Allowed {
            cfg: "#[cfg(feature = \"management\")]",
            target: "register_jmx_natives(registry);",
            reason: "`management` is a DEFAULT feature of cratonvm-vm, so it is                      ON in both the -p cratonvm-cli and --workspace resolves.                      It is opt-OUT (for musl / minimal builds via                      --no-default-features), not opt-in, so it cannot fork CI                      from a suite build the way the experimental-* features do.",
        },
        Allowed {
            cfg: "#[cfg(feature = \"experimental-aot\")]",
            target: "aot::register_aot_natives(registry);",
            reason: "Experimental AOT surface, opt-in by design.",
        },
        Allowed {
            cfg: "#[cfg(feature = \"experimental-aot\")]",
            target: "cds::register_cds_natives(registry);",
            reason: "Experimental CDS surface, opt-in by design.",
        },
        Allowed {
            cfg: "#[cfg(windows)]",
            target: "attach_provider::register_attach_provider(registry);",
            reason: "A TARGET gate, not a feature gate, so it does not fork                      `-p cratonvm-cli` from `--workspace` -- both resolve it                      the same way on the same host. It forks Windows from                      Linux, and that fork is the JDK's:                      `sun/tools/attach/AttachProviderImpl` is a different                      class on each platform, and only the Windows one                      declares `tempPath()Ljava/lang/String;` and                      `volumeFlags(Ljava/lang/String;)J`. Registering them on                      Linux would bind natives to methods that do not exist                      there; the Linux `listVirtualMachines()` is ordinary                      bytecode and needs no native at all. The module itself                      is `#![cfg(windows)]` (native-builtins/src/                     attach_provider.rs), so the gate is also what makes the                      call site compile off-Windows.",
        },
    ];

    let src = read_ws("native-builtins/src/lib.rs");
    let lines: Vec<&str> = src.lines().collect();

    // First source line after `from` that is neither blank nor a `//` comment.
    let next_meaningful = |from: usize| -> Option<(usize, &str)> {
        lines[from..]
            .iter()
            .enumerate()
            .map(|(off, l)| (from + off, l.trim()))
            .find(|(_, l)| !l.is_empty() && !l.starts_with("//"))
    };

    let mut undeclared: Vec<String> = Vec::new();
    let mut declared = 0usize;
    // Which allow-list rows were actually used. A bare count says a row is
    // dead but not which one, and finding it by hand means re-deriving the
    // whole scan by inspection -- which is what the
    // `serialization::register_serialization_natives` row cost, and it had
    // been ungated deliberately and at length.
    let mut matched = vec![false; allowed.len()];

    for (i, line) in lines.iter().enumerate() {
        let cfg = line.trim();
        if !cfg.starts_with("#[cfg(") {
            continue;
        }
        let Some((at, mut target)) = next_meaningful(i + 1) else {
            continue;
        };
        // A gate can wrap a braced block of registrations rather than a single
        // call — `#[cfg(...)] { registry.register(...); ... }`. Step into it, or
        // the gate reads as "not a registration" and is skipped silently, which
        // is the exact blindness this test exists to remove.
        if target == "{" {
            match next_meaningful(at + 1) {
                Some((_, inner)) => target = inner,
                None => continue,
            }
        }
        let is_registration = target.starts_with("registry.register(")
            || (target.contains("register") && target.contains("(registry)"));
        if !is_registration {
            continue;
        }
        if let Some(k) = allowed
            .iter()
            .position(|a| a.cfg == cfg && a.target == target)
        {
            declared += 1;
            matched[k] = true;
            continue;
        }
        undeclared.push(format!("line {}: {cfg}  ->  {target}", i + 1));
    }

    assert!(
        undeclared.is_empty(),
        "T14: {} native registration(s) sit behind a #[cfg(...)] that is not on \
         this test's allow-list:\n{}\n\nA gated registration is absent from \
         `cargo build -p cratonvm-cli` (every suite runner) and present in \
         `cargo build --workspace` (CI), so it forks the two builds silently. \
         Either remove the gate, or add an entry to `allowed` recording why \
         this native may legitimately differ between builds.",
        undeclared.len(),
        undeclared.join("\n"),
    );

    // Every allow-list entry must still correspond to real source. A stale
    // entry would let a future gate of the same shape pass unnoticed.
    let dead: Vec<String> = allowed
        .iter()
        .zip(&matched)
        .filter(|(_, &m)| !m)
        .map(|(a, _)| format!("  {}  ->  {}", a.cfg, a.target))
        .collect();
    assert!(
        dead.is_empty(),
        "T14: {} allow-list entr{} no longer matching any source line. The gate \
         each excuses is gone, so it would silently excuse the next gate of \
         the same shape. Delete them, or fix their `cfg`/`target` text:\n{}",
        dead.len(),
        if dead.len() == 1 { "y" } else { "ies" },
        dead.join("\n"),
    );
    eprintln!("[T14.3c] \u{2713} All {declared} gated native registrations are declared");
}

// ===========================================================================
// T14.4 — VM.getSavedProperty uses real implementation (not stub)
// ===========================================================================

#[test]
fn t14_vm_get_saved_property_not_stub() {
    let compact = compact_ws(&read_ws("native-builtins/src/lib.rs"));

    // Find the getSavedProperty registration handler (any line-wrapping).
    let args = reg_handler(
        &compact,
        "jdk/internal/misc/VM",
        "getSavedProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
    )
    .expect("getSavedProperty registration not found");

    // Must NOT be an inline stub closure (whitespace-stripped form).
    assert!(
        !args.contains("|_ctx,_args|"),
        "T14: VM.getSavedProperty still uses a stub closure",
    );
    // Must point to the real implementation
    assert!(
        args.contains("native_vm_get_saved_property"),
        "T14: VM.getSavedProperty not wired to native_vm_get_saved_property",
    );
    eprintln!("[T14.4] ✓ VM.getSavedProperty uses real implementation");
}

// ===========================================================================
// T14.5 — VM.getRuntimeArguments uses real implementation
// ===========================================================================

#[test]
fn t14_vm_get_runtime_arguments_not_stub() {
    let compact = compact_ws(&read_ws("native-builtins/src/lib.rs"));

    let args = reg_handler(
        &compact,
        "jdk/internal/misc/VM",
        "getRuntimeArguments",
        "()[Ljava/lang/String;",
    )
    .expect("getRuntimeArguments registration not found");

    // Must NOT return null and must be wired to the named function.
    assert!(
        !args.contains("Object(None)"),
        "T14: VM.getRuntimeArguments still returns null",
    );
    assert!(
        args.contains("native_vm_get_runtime_arguments"),
        "T14: VM.getRuntimeArguments not wired to named function",
    );
    eprintln!("[T14.5] ✓ VM.getRuntimeArguments returns empty array (not null)");
}

// ===========================================================================
// T14.6 — Named implementations exist in lang_system.rs
// ===========================================================================

const T14_IMPLEMENTATIONS: &[&str] = &[
    "native_system_init_phase1",
    "native_system_init_phase2",
    "native_system_init_phase3",
    "native_vm_get_saved_property",
    "native_vm_get_runtime_arguments",
];

#[test]
fn t14_named_implementations_exist() {
    let src = read_ws("native-builtins/src/lang_system.rs");
    let mut missing: Vec<&str> = Vec::new();

    for &func in T14_IMPLEMENTATIONS {
        let pattern = format!("fn {func}(");
        if !src.contains(&pattern) {
            missing.push(func);
        }
    }

    assert!(
        missing.is_empty(),
        "T14: {} implementations missing in lang_system.rs:\n  {}",
        missing.len(),
        missing.join("\n  "),
    );
    eprintln!(
        "[T14.6] ✓ All {}/{} T14 implementations found in lang_system.rs",
        T14_IMPLEMENTATIONS.len(),
        T14_IMPLEMENTATIONS.len(),
    );
}

// ===========================================================================
// T14.7 — Unit tests exist for T14 methods
// ===========================================================================

#[test]
fn t14_unit_tests_exist() {
    let src = read_ws("native-builtins/src/lang_system.rs");

    let required_test_patterns = [
        "init_phase1_succeeds",
        "init_phase2_returns_zero",
        "init_phase3_succeeds",
        "vm_get_saved_property_returns_null_for_missing",
        "vm_get_saved_property_returns_value",
        "vm_get_runtime_arguments_returns_empty_array",
    ];

    let mut missing: Vec<&str> = Vec::new();
    for &pattern in &required_test_patterns {
        if !src.contains(pattern) {
            missing.push(pattern);
        }
    }

    assert!(
        missing.is_empty(),
        "T14: {} unit test functions missing:\n  {}",
        missing.len(),
        missing.join("\n  "),
    );
    eprintln!(
        "[T14.7] ✓ All {}/{} T14 unit test patterns found",
        required_test_patterns.len(),
        required_test_patterns.len(),
    );
}

// ===========================================================================
// T14.8 — No stub patterns in System registrations
// ===========================================================================

#[test]
fn t14_no_stubs_in_system_registrations() {
    let lib = read_ws("native-builtins/src/lib.rs");
    let stub_markers = ["todo!()", "unimplemented!()", "STUB"];
    let mut stubs: Vec<String> = Vec::new();

    for line in lib.lines() {
        if !line.contains("\"java/lang/System\"") || !line.contains("register") {
            continue;
        }
        let lower = line.to_lowercase();
        for marker in &stub_markers {
            if lower.contains(&marker.to_lowercase()) {
                stubs.push(line.trim().to_string());
                break;
            }
        }
    }

    assert!(
        stubs.is_empty(),
        "T14: stub patterns found in System registrations:\n  {}",
        stubs.join("\n  "),
    );
    eprintln!("[T14.8] ✓ No stub patterns in java/lang/System registrations");
}

// ===========================================================================
// T14.9 — System properties are populated at VM init
// ===========================================================================

#[test]
fn t14_system_properties_populated() {
    let init = read_ws("vm/src/vm/vm_init.rs");

    let required_properties = [
        "os.name",
        "os.arch",
        "file.separator",
        "path.separator",
        "line.separator",
        "java.version",
        "java.vendor",
        "file.encoding",
        "user.dir",
        "java.io.tmpdir",
    ];

    let mut missing: Vec<&str> = Vec::new();
    for &prop in &required_properties {
        let pattern = format!("\"{}\"", prop);
        if !init.contains(&pattern) {
            missing.push(prop);
        }
    }

    assert!(
        missing.is_empty(),
        "T14: {} system properties not populated in vm_init.rs:\n  {}",
        missing.len(),
        missing.join("\n  "),
    );
    eprintln!(
        "[T14.9] ✓ All {}/{} system properties populated at VM init",
        required_properties.len(),
        required_properties.len(),
    );
}

// ===========================================================================
// T14.10 — System.out/err intercept exists in interpreter
// ===========================================================================

#[test]
fn t14_system_stream_intercept_exists() {
    // The whole interpreter module: `ensure_system_streams` moved into
    // `interpreter/opcodes.rs` with `execute_instruction` in the SEAM-02 split,
    // and a check that reads only `interpreter.rs` would report the intercept
    // missing when it had merely moved one file over.
    let interp = read_module("vm/src/runtime/interpreter.rs");

    assert!(
        interp.contains("System.out") || interp.contains("java/lang/System"),
        "T14: interpreter missing System.out/err intercept",
    );
    assert!(
        interp.contains("ensure_system_streams"),
        "T14: interpreter missing ensure_system_streams call",
    );
    eprintln!("[T14.10] ✓ System.out/err stream intercept present in interpreter");
}
