/// The registrar source, read from the WORKING TREE at test time (not
/// `include_str!`, which would freeze a compile-time snapshot and let the
/// witness pass against source that is no longer there).
const VM_INIT_RS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/vm/vm_init/real_jdk_to_array_typed.rs");
const VM_CARGO_TOML: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");

/// Opens the synthetic-JDK **mode** arm (runtime flag, not a Cargo cfg).
const SYNTHETIC_ARM_OPEN: &str = "if config.use_synthetic_jdk {";
/// Opens real-JDK arm A — compiled when `synthetic-jdk` is ON.
const REAL_ARM_A_OPEN: &str = "} else {";
/// Opens real-JDK arm B — compiled when `synthetic-jdk` is OFF.
const REAL_ARM_B_OPEN: &str = "#[cfg(not(feature = \"synthetic-jdk\"))]";
/// Final statement of BOTH real-JDK arms. Matched against the whole
/// trimmed line, so this constant's own declaration cannot match it.
const REAL_ARM_CLOSE: &str = "\"Real JDK mode: {} native methods registered\",";

fn read(path: &str) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("witness must read `{}` from the working tree: {}", path, e))
}

/// The first argument every registration pass takes: the registry being
/// built.
const REGISTRY_ARG: &str = "&mut native_methods";

/// Every registration pass called in `lines`, in source order, named by
/// the last segment of its path.
///
/// A pass is any call whose first argument is the registry. Keying on the
/// ARGUMENT rather than on a `register_` name prefix is deliberate:
/// `init_service_loader_bootstrap` is a registration pass too, and the
/// first draft of this witness — a name-prefix scanner — silently skipped
/// it. That is the same "trace it, do not infer from names" failure the
/// witness exists to stop.
///
/// Method calls (`native_methods.register(...)`) are excluded
/// because their receiver is not an argument; whole-line `//` comments are
/// skipped, so back-ticked prose mentions never match. rustfmt moves the
/// argument to the next line for the longer paths, so both spellings are
/// accepted.
fn registration_passes<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    let mut found: Vec<&'a str> = Vec::new();
    for (i, &line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        let bytes = line.as_bytes();
        for (open, _) in line.char_indices().filter(|(_, c)| *c == '(') {
            let mut start = open;
            while start > 0 {
                let p = bytes[start - 1];
                if p == b'_' || p == b':' || p.is_ascii_alphanumeric() {
                    start -= 1;
                } else {
                    break;
                }
            }
            if start == open || (start > 0 && bytes[start - 1] == b'.') {
                continue;
            }
            let after = line[open + 1..].trim_start();
            let takes_registry = if after.is_empty() {
                lines
                    .get(i + 1)
                    .is_some_and(|n| n.trim_start().starts_with(REGISTRY_ARG))
            } else {
                after.starts_with(REGISTRY_ARG)
            };
            if takes_registry {
                let path = &line[start..open];
                found.push(path.rsplit("::").next().unwrap_or(path));
            }
        }
    }
    found
}

fn only(lines: &[&str], trimmed: &str) -> usize {
    let hits: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim() == trimmed)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "the registrar-arm anchor `{}` must occur exactly once in vm_init.rs; found it \
         on lines {:?}. Anchors are how this witness locates the three registration \
         arms — re-anchor the witness in the same change that moved them.",
        trimmed,
        hits.iter().map(|i| i + 1).collect::<Vec<_>>()
    );
    hits[0]
}

struct Arms<'a> {
    /// synthetic-JDK mode, `synthetic-jdk` feature ON (configuration 3).
    synthetic: Vec<&'a str>,
    /// real-JDK mode, `synthetic-jdk` feature ON (configuration 1) — also
    /// the arm `--real-jdk` takes in a feature-enabled build.
    real_feature_on: Vec<&'a str>,
    /// real-JDK mode, `synthetic-jdk` feature OFF (configuration 2), the
    /// shipping `cratonvm-cli` arm. Also configuration 4: a feature-OFF
    /// build that was *asked* for synthetic mode lands here anyway,
    /// because this block never reads `config.use_synthetic_jdk`.
    real_feature_off: Vec<&'a str>,
    b_open: usize,
    b_close: usize,
}

fn arms(src: &str) -> Arms<'_> {
    let lines: Vec<&str> = src.lines().collect();
    let syn_open = only(&lines, SYNTHETIC_ARM_OPEN);
    let a_open = lines
        .iter()
        .enumerate()
        .skip(syn_open + 1)
        .find(|(_, l)| l.trim() == REAL_ARM_A_OPEN)
        .map(|(i, _)| i)
        .expect("real-JDK arm A is the `else` of `if config.use_synthetic_jdk`");
    let closes: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim() == REAL_ARM_CLOSE)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        closes.len(),
        2,
        "exactly two real-JDK arms must exist, each ending in the `Real JDK mode: ...` \
         tracing line; found {} such lines: {:?}",
        closes.len(),
        closes.iter().map(|i| i + 1).collect::<Vec<_>>()
    );
    let b_open = only(&lines, REAL_ARM_B_OPEN);
    assert!(
        syn_open < a_open && a_open < closes[0] && closes[0] < b_open && b_open < closes[1],
        "registration arms are out of their expected source order: synthetic@{} \
         realA@{} realA-end@{} realB@{} realB-end@{}",
        syn_open + 1,
        a_open + 1,
        closes[0] + 1,
        b_open + 1,
        closes[1] + 1
    );
    Arms {
        synthetic: registration_passes(&lines[syn_open..a_open]),
        real_feature_on: registration_passes(&lines[a_open..closes[0]]),
        real_feature_off: registration_passes(&lines[b_open..closes[1]]),
        b_open,
        b_close: closes[1],
    }
}

fn position(list: &[&str], name: &str) -> usize {
    list.iter().position(|c| *c == name).unwrap_or_else(|| {
        panic!(
            "`{}` is no longer called from this real-JDK arm. Registration is \
             last-write-wins, so dropping a registrar silently hands its triples to \
             whichever earlier pass registered them. If the removal is deliberate, \
             drop the ordering claim here in the same change.",
            name
        )
    })
}

fn must_precede(list: &[&str], arm: &str, earlier: &str, later: &str, why: &str) {
    let e = position(list, earlier);
    let l = position(list, later);
    assert!(
        e < l,
        "{}: `{}` (position {}) must run BEFORE `{}` (position {}). {}",
        arm,
        earlier,
        e,
        later,
        l,
        why
    );
}

/// The drift this lane was opened for: the two real-JDK arms had grown
/// apart. Arm A was missing `register_classvalue_natives`,
/// `register_random_and_securerandom_natives` (a seeded `Random` returned
/// all zeros there), `register_phase57_file` and
/// `register_spring_boot_logback_apply`.
///
/// Sequence equality, not set equality: order is semantics here.
#[test]
fn the_two_real_jdk_arms_run_the_same_registrars_in_the_same_order() {
    let src = read(VM_INIT_RS);
    let a = arms(&src);
    assert_eq!(
        a.real_feature_on, a.real_feature_off,
        "the two real-JDK arms of vm_init must call the same registrars in the same \
         order. They differ only in which Cargo feature COMPILED them, never in which \
         class library is loaded, so a registrar in one and not the other is a \
         mode-specific defect: the `--features synthetic-jdk` build's real-JDK and \
         `--real-jdk` runs take the first list, every shipping `cratonvm-cli` run takes \
         the second."
    );
    assert!(
        a.real_feature_on.len() > 40,
        "the real-JDK registrar sequence collapsed to {} entries — the witness is \
         almost certainly parsing the wrong line range rather than seeing a real \
         deletion",
        a.real_feature_on.len()
    );
}

/// Confusions 1 and 2 above, stated where they can be checked: the
/// synthetic-override family (and with it panama, phase 64 and phase 67)
/// is reachable ONLY through `register_builtins`, and only the
/// synthetic-MODE arm calls it. A fix landed inside
/// `register_synthetic_overrides` does not reach `--real-jdk`.
#[test]
fn only_the_synthetic_mode_arm_reaches_register_builtins() {
    let src = read(VM_INIT_RS);
    let a = arms(&src);
    assert_eq!(
        a.synthetic.first(),
        Some(&"register_builtins"),
        "the synthetic-mode arm must open with `register_builtins` (essentials, then \
         `register_synthetic_overrides`); it opened with {:?}",
        a.synthetic.first()
    );
    for (arm, calls) in [
        ("real-JDK arm A (feature ON)", &a.real_feature_on),
        ("real-JDK arm B (feature OFF)", &a.real_feature_off),
    ] {
        for forbidden in ["register_builtins", "register_synthetic_overrides"] {
            assert!(
                !calls.contains(&forbidden),
                "{} must never call `{}`: it pulls in `register_synthetic_overrides`, \
                 whose rows assume synthetic field layouts and corrupt real JDK \
                 objects. It is also the ONLY caller of that function, which is why a \
                 registrar reached only from there (panama's `register_pe_panama`, the \
                 phase-64/67 families) is synthetic-mode-only however its `NativeKind` \
                 is tagged.",
                arm,
                forbidden
            );
        }
    }
    assert!(
        !a.synthetic
            .contains(&"register_essential_natives_with_shims"),
        "the synthetic arm must not re-run essentials after `register_builtins`: \
         registration is last-write-wins, so it would demote every synthetic override \
         back to its essential twin."
    );
}

/// Confusion 4, plus the ordering claims the surrounding comments make in
/// prose. Every pair below is an incident, not a preference.
#[test]
fn last_write_wins_ordering_holds_inside_both_real_jdk_arms() {
    let src = read(VM_INIT_RS);
    let a = arms(&src);
    for (arm, calls) in [
        ("real-JDK arm A (feature ON)", &a.real_feature_on),
        ("real-JDK arm B (feature OFF)", &a.real_feature_off),
    ] {
        must_precede(
            calls,
            arm,
            "register_essential_natives_with_shims",
            "register_io_natives",
            "`native-io`'s bodies deliberately SHADOW `native-builtins`' aliasing \
             implementations for the java.io surface. A clearance measured against the \
             builtins body is measuring code this arm never runs.",
        );
        must_precede(
            calls,
            arm,
            "register_concurrent_natives",
            "register_forkjoin_quiescence",
            "`register_concurrent_natives` registers the constant \
             `ForkJoinPool.awaitQuiescence -> true`; the real one polls this crate's \
             async worker pool and must overwrite it.",
        );
        must_precede(
            calls,
            arm,
            "register_collections_natives",
            "register_random_and_securerandom_natives",
            "`register_collections_natives` re-registers the layout-dependent \
             `java/util/Random` aliases, which read field 0 as a long when in real-JDK \
             mode it is the `AtomicLong seed` REFERENCE — a seeded `Random` then emits \
             all zeros.",
        );
        must_precede(
            calls,
            arm,
            "register_collections_natives",
            "register_properties_sidetable",
            "`register_collections_natives` re-registers `Properties.load` / \
             `getProperty` / `setProperty` with the legacy HashMap-layout natives, \
             overwriting the side-table-backed pair Surefire's `loadProperties` \
             round-trip needs.",
        );
        for (earlier, later) in [
            ("register_phase57_nio_file", "register_phase57_file"),
            ("register_phase57_file", "register_p59_jar"),
            ("register_p59_jar", "register_p59_bulk_stream_transfer"),
            (
                "register_p59_bulk_stream_transfer",
                "register_p59_zip_output_primitives",
            ),
        ] {
            must_precede(
                calls,
                arm,
                earlier,
                later,
                "the file/jar/zip family is registered nio_file -> file -> jar -> bulk \
                 -> zip-output; re-ordering it hands `JarFile.entries()` back to \
                 `native-io`'s `alloc_zip_entry`, which answers `ZipEntry` where the \
                 declared `Enumeration<JarEntry>` checkcast demands `JarEntry`.",
            );
        }
    }
}

/// Real-JDK arm A leaves its `sun.management` / JMX registrars UNGATED
/// while arm B gates each one on `#[cfg(feature = "management")]`. That is
/// sound only because `synthetic-jdk` — the feature that compiles arm A —
/// itself enables `management`. Nothing said so; now something checks it.
/// If the implication is ever dropped, arm A stops COMPILING (loud), which
/// is why the cfg attributes are deliberately not mirrored onto it.
#[test]
fn the_synthetic_jdk_feature_still_implies_management() {
    let toml = read(VM_CARGO_TOML);
    let start = toml
        .find("\nsynthetic-jdk = [")
        .expect("vm/Cargo.toml must declare a multi-line `synthetic-jdk` feature list");
    let rest = &toml[start..];
    let end = rest
        .find("\n]")
        .expect("the `synthetic-jdk` feature list must be closed by a `]` at column 0");
    let list = &rest[..end];
    assert!(
        list.contains("\"management\""),
        "`synthetic-jdk` must keep enabling `management`. vm_init's real-JDK arm A \
         (the `else` of `if config.use_synthetic_jdk`, compiled only under \
         `synthetic-jdk`) calls the `jmx::register_*_impl` family WITHOUT a \
         `#[cfg(feature = \"management\")]` gate, unlike arm B. The list read:\n{}",
        list
    );
}

/// The fourth configuration, recorded where it can rot loudly: a build
/// WITHOUT `synthetic-jdk` that is asked for synthetic mode still lands in
/// real-JDK arm B, because that block calls the same registration passes
/// either way. `require_synthetic_jdk` rejects that pairing — but only on
/// the CLI / `libcratonvm` entry paths, not in `SharedVm::new`, so an
/// embedder (and `VmConfig::default()`, whose JDK mode is Synthetic)
/// reaches it.
///
/// 2026-09-05: this test used to require that arm B never read
/// `config.use_synthetic_jdk` at all. It now reads it in EXACTLY one
/// place, and this test pins that it stays exactly one, and stays that one.
///
/// The half that was load-bearing is unchanged, and the count assertion is
/// still what protects it: no `register_*` pass in arm B is conditional,
/// so the configuration above gets the whole real-JDK registrar set rather
/// than an empty registry. What the one branch decides is
/// `set_drop_real_layout_synthetic`, which is not a registration — it is
/// the policy `register` then applies to them — and setting it
/// unconditionally deleted `java/lang/String`'s ONLY implementation in
/// exactly the configuration this comment describes: no bytecode (no JDK
/// image) and no bridge (dropped), so `"abcdef".length()` raised
/// `NoSuchMethodError`.
#[test]
fn the_feature_off_arm_consults_the_runtime_jdk_mode_only_for_the_layout_drop() {
    let src = read(VM_INIT_RS);
    let a = arms(&src);
    let lines: Vec<&str> = src.lines().collect();
    let offenders: Vec<usize> = (a.b_open..a.b_close)
        .filter(|i| {
            let l = lines[*i];
            !l.trim_start().starts_with("//") && l.contains("use_synthetic_jdk")
        })
        .map(|i| i + 1)
        .collect();
    assert_eq!(
        offenders.len(),
        1,
        "the `#[cfg(not(feature = \"synthetic-jdk\"))]` arm branches on \
         `config.use_synthetic_jdk` at lines {:?}. Exactly ONE such branch is allowed, \
         and it is the `set_drop_real_layout_synthetic` decision; a second one creates a \
         fourth registration path — update the F30 census in the same change.",
        offenders
    );
    let i = offenders[0] - 1;
    let got: Vec<&str> = lines[i..i + 3].iter().map(|l| l.trim()).collect();
    assert_eq!(
        got,
        vec![
            "if !config.use_synthetic_jdk {",
            "native_methods.set_drop_real_layout_synthetic(true);",
            "}",
        ],
        "arm B's one runtime-mode branch is no longer the layout-drop decision — it \
         reads {:?}. Either restore it, or decide in the F30 census what the new branch \
         is and why arm B may have it.",
        got
    );
}
