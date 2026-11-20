// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! An inline intrinsic that claims an `invokevirtual` inside a protected range
//! must still deliver that site's NPE to the method's own handler, with the
//! handler's locals live.
//!
//! `compile_osr_artifact` admits a method with a non-empty exception table on
//! one promise (RBC.6b): every throwing site inside a protected range publishes
//! a reason-9 (`PendingException`) precise frame, so
//! `route_osr_exception_out_of_artifact` may deduce that no stashed frame means
//! the throw was outside every protected range, and propagate. The predicate
//! that checks it, `first_unsupported_precise_frame_site`, reads the BYTECODE,
//! and clears opcode `0xb6` because `precise_frame_publishing_opcode` says the
//! ORDINARY invokevirtual lowering publishes.
//!
//! The BOX_UNBOX region then substitutes its own lowering for
//! `Integer.intValue` / `Long.longValue`, whose null-receiver edge is a
//! reason-6 (`ReceiverTypeChanged`) deopt stub rather than a reason-9
//! publication. That is sound, for a reason nothing else in the tree records: a
//! reason-6 deopt is not a weaker publication, it is a STRONGER action — it
//! abandons the compiled frame and hands a reconstructed one back to the
//! interpreter, which raises the NPE and searches the exception table itself.
//! The reason-9 frame is for an exception arriving INSIDE the compiled body
//! that has to be routed without leaving it.
//!
//! This pins that. If the intrinsic ever grows an edge that stays in compiled
//! code without publishing, the handler is skipped or entered on stale locals.
//! `internal/fixed-bugs/jit-superseded-implicit-npe-leak-FIXED-20260903.md`
//! records the reading that was retracted to get here.
//!
//! # Why this asserts ENGAGEMENT and not only the answer
//!
//! Every property this probe depends on can be lost silently, and each loss
//! leaves it printing `PASS` while measuring nothing:
//!
//! * one `putstatic`, array load or `ldc` added inside the `try` refuses the
//!   whole method for OSR (`first_unsupported_precise_frame_site`), so no
//!   compiled body exists to be wrong;
//! * the BOX_UNBOX family is a default that has been flipped twice, and with it
//!   off the site is an ordinary CALL and the substitution under test never
//!   happens.
//!
//! So the run is made under `CRATONVM_DBG_JITC=1` and
//! `CRATONVM_DBG_ATOMIC_INTRINSIC=1`, and both witnesses are asserted: the OSR
//! compile of `arm()V`, and the intrinsic resolving `Integer.intValue()I` — a
//! site that exists nowhere in the fixture but inside that `try`.
//!
//! Both witnesses were checked against a run that should LOSE them, because an
//! engagement assertion nobody has seen fail is an engagement assertion nobody
//! has tested:
//!
//! | arm | box-unbox witness | OSR witness | probe answer |
//! |---|---|---|---|
//! | as landed | present | present | correct |
//! | `CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC=1` | **gone** | present | correct |
//! | a `putstatic` planted inside the `try` | present | **gone** | correct |
//!
//! The third row is why the OSR check is spelled the way it is. Its first
//! spelling — `contains("OSR-compile") && contains("...arm()V")` — also matched
//! `OSR-compile FAILED ...arm()V ... method marked OSR-denied`, the exact
//! negative it existed to catch, and passed on that row. A substring witness
//! that matches its own negation is worse than none: it reads as coverage.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod common;

/// The oracle line, produced by a real JDK 25 running the byte-identical
/// fixture. `caught` is `200_000 / 500`; `bad` and `drift` are the handler's
/// own reads of its pre-loop locals.
const ORACLE: &str = "caught=400 bad=0 drift=0 escaped=0 sum=6486400";

/// 200 000 iterations of a boxed add finish in about a second. Generous,
/// because a loaded CI host is exactly when it is slowest.
const TIMEOUT: Duration = Duration::from_secs(300);

fn probe_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("osr_unbox_intrinsic_handler_frame_fixtures")
}

fn cratonvm_binary() -> Option<PathBuf> {
    common::require_binary(cratonvm_binary_lookup())
}

fn cratonvm_binary_lookup() -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("CRATONVM_BIN") {
        let p = PathBuf::from(&bin);
        if p.exists() {
            return Some(p);
        }
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = manifest.parent().unwrap().join("target");
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    for profile in &["release", "debug"] {
        let candidate = target.join(profile).join(exe);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Compile the fixture if the checked-in `.class` is missing.
///
/// A `javac` that cannot be LAUNCHED is the one legitimate skip. A `javac` that
/// ran and rejected the source is a broken fixture and must fail loudly —
/// reporting that as "toolchain missing" is how a test becomes a permanent
/// vacuous pass.
fn ensure_probe_compiled() -> bool {
    let dir = probe_dir();
    let class_file = dir.join("UnboxPreOsrLocalProbe.class");
    if class_file.exists() {
        return true;
    }
    let source = dir.join("UnboxPreOsrLocalProbe.java");
    if !source.exists() {
        return false;
    }
    let compile = Command::new("javac")
        .arg("--release")
        .arg("21")
        .arg("-d")
        .arg(&dir)
        .arg(&source)
        .output();
    match compile {
        Err(_) => false,
        Ok(o) => {
            if !o.status.success() {
                let stderr = String::from_utf8_lossy(&o.stderr);
                if stderr.contains("release version") && stderr.contains("not supported") {
                    eprintln!(
                        "[osr_unbox_intrinsic_handler_frame] javac cannot target --release 21; \
                         skipping. Point JAVA_HOME at a JDK 21+ install."
                    );
                    return false;
                }
            }
            assert!(
                o.status.success(),
                "[osr_unbox_intrinsic_handler_frame] the checked-in fixture failed to compile \
                 — fix the .java source. javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            class_file.exists()
        }
    }
}

/// `(stdout, stderr)`. `None` means a prerequisite was missing (and
/// `CRATONVM_REQUIRE_E2E` was not set).



/// The `jitc`/intrinsic lines only — a failing assertion above wants the
/// compile decisions, not several megabytes of GC trace.
fn jitc_lines(stderr: &str) -> String {
    stderr
        .lines()
        .filter(|l| l.contains("UnboxPreOsrLocalProbe") || l.contains("[box-unbox-intrinsic]"))
        .take(40)
        .collect::<Vec<_>>()
        .join("\n")
}
