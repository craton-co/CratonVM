// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! End-to-end regressions for two JIT defects that produced an out-of-bounds
//! ELEMENT ADDRESS — a heap access at an address the bounds check had just
//! approved, which is a JVM safety-property violation rather than a wrong
//! answer.
//!
//! Both were found by review rather than by a failing test, and NEITHER could
//! have been caught by the differential harness the JIT already had, which is
//! the reason this file exists at the end-to-end level:
//!
//!   * **The BCE `iinc` defect.** `analyze_array_access_operands` modelled
//!     `iinc` as a no-op — true of the operand stack, false of the value — so
//!     an index read AFTER an in-body advance was described to the proof as
//!     the bare induction variable. `while (i < n) { i++; a[i] = i; }` called
//!     with `new int[8], 8` passed its `a.length >= n` header guard, had its
//!     per-element check elided, and stored to `a[8]`. A unit test over the
//!     analyzer catches the analysis; only a real run catches the store.
//!
//!   * **The dirty-high-half defect.** `Op::And`/`Or`/`Xor` in the optimizing
//!     tier hard-coded REX.W, so a 64-bit bitwise op over one sign-extended
//!     and one zero-extended operand produced a word that is neither
//!     extension of its own low half (e.g. `0xFFFF_FFFF_0000_0006`). The
//!     bounds check is a 32-bit `CMP ECX, R10D`; the element access scales
//!     all 64 bits of RCX as the SIB index. The single-pass tier is NOT
//!     affected, so `ir_vs_singlepass` could not see it: both tiers agree on
//!     the low 32 bits and only the ADDRESS diverges. Two tiers agreeing on a
//!     wrong-address bug is exactly the blind spot a differential comparison
//!     has by construction.
//!
//! Each probe is warmed hard enough to reach the optimizing tier and then
//! asserted on observable Java behaviour: the right exception, at the right
//! index, with neighbouring memory untouched.
//!
//! **Warming is not enough for defect 2, and this file used to assume it
//! was.** `xorThenIndex` and its two siblings are small, loop-free leaves, so
//! under the default `CRATONVM_C2_ACCEPT=evidence` the acceptance gate refuses
//! their IR bodies for want of a transform worth replacing the baseline with,
//! and every call runs the single-pass artifact -- which never had the bug.
//! The default arm therefore proved nothing about defect 2. The
//! `forced_open` arm publishes every IR body and then asserts, BY METHOD NAME,
//! that each of the three took the tier. See
//! `docs/internal/fixed-bugs/jit/ir-tier-admission-hides-its-own-defects-FIXED-20260918.md`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const PROBE_SRC: &str = r#"
public class JitOobElementAddressProbe {

    // ---- Defect 1: an index advanced before the access. ----------------
    //
    // The exit test bounds `i`; the access reads `i + 1`. A guard proving
    // `a.length >= n` justifies nothing about index `n`.
    static int advanceThenStore(int[] a, int n) {
        int i = 0;
        int last = -1;
        while (i < n) {
            i++;
            a[i] = i;      // a[n] on the final iteration
            last = i;
        }
        return last;
    }

    // The `i < a.length` spelling, which reached BoundsProof::Static — the
    // check vanished with no guard at all.
    static int advanceThenStoreAgainstLength(int[] a) {
        int i = 0;
        int last = -1;
        while (i < a.length) {
            i++;
            a[i] = i;
            last = i;
        }
        return last;
    }

    // The ordinary javac counted-loop shape, which must KEEP working and
    // must keep being optimized: the advance happens after the access, so
    // the displacement is zero and BCE still applies.
    static long normalCountedLoop(int[] a) {
        long sum = 0;
        for (int i = 0; i < a.length; i++) {
            a[i] = i;
            sum += a[i];
        }
        return sum;
    }

    // ---- Defect 2: a bitwise op leaving a dirty high half. --------------
    //
    // `src[0]` is negative, so `iaload` sign-extends it. `t` is a 32-bit ALU
    // result, so it is zero-extended. A REX.W XOR of the two produced
    // 0xFFFF_FFFF_0000_0006 for a low half of 6.
    static int xorThenIndex(int[] dst, int[] src, int p, int q) {
        int t = p + q;
        int i = src[0] ^ t;
        return dst[i];
    }

    static int andThenIndex(int[] dst, int[] src, int mask) {
        int i = src[0] & mask;
        return dst[i];
    }

    static int orThenIndex(int[] dst, int[] src, int bits) {
        int i = src[0] | bits;
        return dst[i];
    }

    public static void main(String[] args) {
        // ---- Defect 1 ----------------------------------------------------
        //
        // Warm on an array with room to spare, so the warm-up itself never
        // goes out of bounds and the method reaches the optimizing tier
        // with the elision in place.
        int[] roomy = new int[4096];
        long warm = 0;
        for (int r = 0; r < 60000; r++) {
            warm += advanceThenStore(roomy, 16);
        }
        System.out.println("advanceWarmOK=" + (warm > 0));

        // The exact shape. a.length == n, so the last iteration addresses
        // a[n], which is one past the end.
        String advanceExc = "none";
        int[] tight = new int[8];
        try {
            advanceThenStore(tight, 8);
        } catch (ArrayIndexOutOfBoundsException e) {
            advanceExc = "aioobe";
        }
        System.out.println("advanceExc=" + advanceExc);

        // Nothing beyond the array may have been written. A canary array
        // allocated right after `tight` is the neighbour an out-of-bounds
        // store is most likely to land in.
        int[] canary = new int[8];
        boolean canaryClean = true;
        for (int k = 0; k < canary.length; k++) {
            if (canary[k] != 0) {
                canaryClean = false;
            }
        }
        System.out.println("advanceCanaryClean=" + canaryClean);

        // This one is out of bounds for EVERY array by construction, so the
        // warm-up has to absorb the throw to get the method hot at all.
        long warm2 = 0;
        for (int r = 0; r < 3000; r++) {
            try {
                warm2 += advanceThenStoreAgainstLength(roomy);
            } catch (ArrayIndexOutOfBoundsException e) {
                warm2++;
            }
        }
        System.out.println("advanceLenWarmOK=" + (warm2 > 0));

        String advanceLenExc = "none";
        try {
            advanceThenStoreAgainstLength(new int[8]);
        } catch (ArrayIndexOutOfBoundsException e) {
            advanceLenExc = "aioobe";
        }
        System.out.println("advanceLenExc=" + advanceLenExc);

        // The shape that must NOT regress.
        int[] normal = new int[64];
        long nsum = 0;
        for (int r = 0; r < 60000; r++) {
            nsum = normalCountedLoop(normal);
        }
        // 0 + 1 + ... + 63
        System.out.println("normalSum=" + nsum);

        // ---- Defect 2 ----------------------------------------------------
        int[] dst = new int[16];
        for (int k = 0; k < dst.length; k++) {
            dst[k] = 1000 + k;
        }
        int[] src = new int[] { -5 };

        // -5 ^ -3 == 6. Warm, then check the value AND that it did not fault.
        long xw = 0;
        for (int r = 0; r < 60000; r++) {
            xw += xorThenIndex(dst, src, -1, -2);
        }
        System.out.println("xorWarmOK=" + (xw > 0));
        System.out.println("xorValue=" + xorThenIndex(dst, src, -1, -2));

        // -5 & 15 == 11.
        long aw = 0;
        for (int r = 0; r < 60000; r++) {
            aw += andThenIndex(dst, src, 15);
        }
        System.out.println("andWarmOK=" + (aw > 0));
        System.out.println("andValue=" + andThenIndex(dst, src, 15));

        // -5 | 0 == -5, which is a NEGATIVE index and must throw rather
        // than address backwards from the array base.
        String orExc = "none";
        long ow = 0;
        for (int r = 0; r < 3000; r++) {
            try {
                ow += orThenIndex(dst, src, 0);
            } catch (ArrayIndexOutOfBoundsException e) {
                orExc = "aioobe";
            }
        }
        System.out.println("orWarmOK=" + (ow == 0));
        System.out.println("orExc=" + orExc);

        System.out.println("PROBE_DONE");
    }
}
"#;

mod common;

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
    let target = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target");
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

fn jdk_home() -> Option<PathBuf> {
    common::require_jdk(jdk_home_lookup())
}

fn jdk_home_lookup() -> Option<PathBuf> {
    for var in &["CRATONVM_TEST_JDK", "JAVA_HOME"] {
        if let Ok(j) = std::env::var(var) {
            let p = PathBuf::from(&j);
            if p.exists() {
                return Some(p);
            }
        }
    }
    for cand in [
        "/home/victor/jdk25",
        "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot",
        "C:/Program Files/Java/jdk-25",
    ] {
        let p = PathBuf::from(cand);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn compile_probe(javac: &Path) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("cratonvm-jit-oob-element-address-probe");
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("JitOobElementAddressProbe.java");
    // Never let a stale .class from an earlier revision stand in for a source
    // that no longer compiles.
    let _ = std::fs::remove_file(dir.join("JitOobElementAddressProbe.class"));
    std::fs::write(&src, PROBE_SRC).expect("write probe source");
    let out = match Command::new(javac)
        .args(["--release", "21", "-d"])
        .arg(&dir)
        .arg(&src)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[jit_oob_element_address] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    if !out.status.success() {
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[jit_oob_element_address] javac cannot target --release 21 ({}); skipping. \
                 Point JAVA_HOME or CRATONVM_TEST_JDK at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && dir.join("JitOobElementAddressProbe.class").exists(),
        "[jit_oob_element_address] the embedded probe failed to compile — fix the probe \
         source. javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

/// Run the probe. `single_pass_only` refuses the optimizing tier, which is
/// what makes the BCE half of this file cover anything: the defect lives in
/// the SINGLE-PASS bounds-check elimination, and after a normal warm-up the
/// optimizing artifact is the one installed by the time the out-of-bounds
/// call happens. A default-configuration run passes against the unfixed
/// compiler for that reason alone, so both arms are asserted.
/// Which way the acceptance gate is set for a run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Gate {
    /// The default, `evidence`.
    Default,
    /// `never`: the single-pass tier only.
    SinglePassOnly,
    /// `always`: every IR body that lowers is published, which is the only
    /// setting under which the small leaf methods of defect 2 reach the tier.
    ForcedOpen,
}

fn run_probe(bin: &Path, jdk: &Path, classes: &Path, gate: Gate) -> (String, String) {
    let mut cmd = Command::new(bin);
    match gate {
        Gate::Default => {}
        Gate::SinglePassOnly => {
            cmd.env("CRATONVM_C2_ACCEPT", "never");
        }
        Gate::ForcedOpen => {
            cmd.env("CRATONVM_C2_ACCEPT", "always");
        }
    }
    cmd.arg("--java-home")
        .arg(jdk)
        // Surfaces "[ir] optimizing backend produced a body for ..." so the
        // anti-vacuity check below can prove the optimizing tier actually ran.
        .env("CRATONVM_DBG", "ir-compiles")
        .arg("-c")
        .arg(classes)
        .arg("JitOobElementAddressProbe")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn cratonvm");
    // `common::wait_draining`, not a `try_wait` poll loop. The loop this
    // replaces read NEITHER pipe until the child had exited, and a piped stream
    // nobody reads blocks its WRITER once the OS buffer fills -- so a probe run
    // under a verbose `CRATONVM_DBG*` flag stalls inside its own exit path and
    // the loop reports "timed out", which reads like the VM hanging rather than
    // like the harness holding it.
    //
    // `wait_draining`'s own doc comment says this shape is "everywhere in
    // `vm/tests`". It was written after `class_loader_unload_regression` lost a
    // session to it (stdout 124 bytes, stderr 101,808) and its closing sentence
    // is the rule: "a test must not depend on the process it drives staying
    // under 64 KiB". Measured on Windows, 2026-09-22: four files carrying this
    // loop failed one `cargo test --workspace` run together, and not one of the
    // failures was about the thing its test names.
    let timeout = Duration::from_secs(300);
    let finished = common::wait_draining(child, timeout);
    assert!(
        !finished.timed_out,
        "[jit_oob_element_address] probe timed out after {timeout:?}"
    );
    let out = finished.output;
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn field(stdout: &str, key: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix(key).map(|v| v.trim().to_string()))
}

/// The methods the optimizing tier published a body for, as
/// `Class.method(descriptor)`, from `CRATONVM_DBG=ir-compiles` output.
fn ir_published(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter_map(|l| {
            l.split("[ir] optimizing backend produced a body for ")
                .nth(1)
        })
        .map(str::trim)
        .collect()
}

#[test]
fn an_index_advanced_before_its_access_keeps_its_bounds_check_in_the_optimizing_tier() {
    check_both_defects(Gate::Default);
}

/// The arm that actually covers the BCE defect. See `run_probe`.
#[test]
fn an_index_advanced_before_its_access_keeps_its_bounds_check_in_the_single_pass_tier() {
    check_both_defects(Gate::SinglePassOnly);
}

/// The arm that actually covers the dirty-high-half defect end to end. See the
/// module doc: under the default gate its three methods never reach the tier.
#[test]
fn a_bitwise_index_keeps_its_high_half_clean_with_the_acceptance_gate_forced_open() {
    check_both_defects(Gate::ForcedOpen);
}

fn check_both_defects(gate: Gate) {
    let Some(bin) = cratonvm_binary() else {
        eprintln!(
            "[jit_oob_element_address] cratonvm binary not found; build it with \
             `cargo build -p cratonvm-cli` (or set CRATONVM_BIN). skipping."
        );
        return;
    };
    let Some(jdk) = jdk_home() else {
        eprintln!(
            "[jit_oob_element_address] no usable JDK found (set CRATONVM_TEST_JDK or \
             JAVA_HOME). skipping."
        );
        return;
    };
    let javac = jdk.join(if cfg!(windows) {
        "bin/javac.exe"
    } else {
        "bin/javac"
    });
    let Some(classes) = compile_probe(&javac) else {
        return;
    };

    let (stdout, stderr) = run_probe(&bin, &jdk, &classes, gate);
    assert!(
        stdout.contains("PROBE_DONE"),
        "[jit_oob_element_address] probe did not reach its final marker — a surviving \
         out-of-bounds access usually shows up here as a crash or a silent hang rather than \
         as a failed assertion.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // ---- Defect 1 --------------------------------------------------------
    assert_eq!(field(&stdout, "advanceWarmOK=").as_deref(), Some("true"));
    assert_eq!(
        field(&stdout, "advanceExc=").as_deref(),
        Some("aioobe"),
        "`while (i < n) {{ i++; a[i] = i; }}` on a HOT method stored past the end instead of \
         throwing: the bounds check was elided on the strength of a guard that bounds `i`, \
         while the access reads `i + 1`.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        field(&stdout, "advanceCanaryClean=").as_deref(),
        Some("true"),
        "the out-of-bounds store reached a neighbouring object.\nstdout:\n{stdout}\n\
         stderr:\n{stderr}"
    );
    assert_eq!(field(&stdout, "advanceLenWarmOK=").as_deref(), Some("true"));
    assert_eq!(
        field(&stdout, "advanceLenExc=").as_deref(),
        Some("aioobe"),
        "the `i < a.length` spelling reached BoundsProof::Static and dropped its check with \
         no guard at all.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // The shape the optimization exists for must still produce the right
    // answer. 0 + 1 + ... + 63 == 2016.
    assert_eq!(
        field(&stdout, "normalSum=").as_deref(),
        Some("2016"),
        "the ordinary counted-loop shape regressed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // ---- Defect 2 --------------------------------------------------------
    assert_eq!(field(&stdout, "xorWarmOK=").as_deref(), Some("true"));
    assert_eq!(
        field(&stdout, "xorValue=").as_deref(),
        // dst[6] == 1006. A dirty high half addressed dst - 17GB instead.
        Some("1006"),
        "`src[0] ^ t` produced an index whose high half disagreed with the low half the \
         bounds check validated.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(field(&stdout, "andWarmOK=").as_deref(), Some("true"));
    assert_eq!(
        field(&stdout, "andValue=").as_deref(),
        // -5 & 15 == 11, so dst[11] == 1011.
        Some("1011"),
        "`src[0] & mask` produced a dirty-high-half index.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(field(&stdout, "orWarmOK=").as_deref(), Some("true"));
    assert_eq!(
        field(&stdout, "orExc=").as_deref(),
        Some("aioobe"),
        "a NEGATIVE index out of `src[0] | 0` did not throw — the unsigned 32-bit bounds \
         compare is what rejects it, and it only works when the high half agrees.\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // Anti-vacuity: every assertion above would pass on the single-pass
    // backend alone, and defect 2 exists ONLY in the optimizing tier. A run
    // that never engaged that tier proves nothing.
    let published = ir_published(&stderr);
    match gate {
        Gate::SinglePassOnly => {
            assert!(
                published.is_empty(),
                "[jit_oob_element_address] `CRATONVM_C2_ACCEPT=never` still published IR \
                 bodies, so the single-pass arm is not single-pass: {published:?}"
            );
        }
        Gate::Default => {
            assert!(
                !published.is_empty(),
                "[jit_oob_element_address] the optimizing tier never compiled anything, so \
                 this probe is vacuous — the dirty-high-half defect lives only in that \
                 tier.\nstderr:\n{stderr}"
            );
        }
        Gate::ForcedOpen => {
            // By name, every one: "some method took the tier" is the check
            // that let this file cover defect 2 without ever running it.
            for method in [
                "JitOobElementAddressProbe.xorThenIndex([I[III)I",
                "JitOobElementAddressProbe.andThenIndex([I[II)I",
                "JitOobElementAddressProbe.orThenIndex([I[II)I",
            ] {
                assert!(
                    published.contains(&method),
                    "[jit_oob_element_address] {method} did not take the optimizing tier \
                     even with the acceptance gate forced open, so its assertions above ran \
                     the single-pass body and cover nothing. Published: {published:?}"
                );
            }
        }
    }
}
