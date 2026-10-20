// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Compile the emitted HIP C++ with a HOST compiler and run it.
//!
//! # Why this exists
//!
//! There is no AMD GPU in this project, so nothing else executes a
//! rendered kernel at all. `render_goldens.rs` is a change detector
//! over the text; `cuda-bridge`'s `hip_module_it` needs hardware. This
//! is the only test that takes the generated source and produces
//! numbers, and it needs nothing but a C++ compiler.
//!
//! It earns its place immediately. The first version of the renderer
//! emitted `char* rd0, rd1, rd2;` — in C++ the `*` binds to the
//! declarator, so every 64-bit register but the first was a single
//! byte. The golden test recorded that happily; a compiler rejects it
//! in a line.
//!
//! # What it can and cannot catch
//!
//! **Catches:** every syntax and type error; the declare-up-front rule
//! that `goto` imposes; index and address arithmetic; the bounds-deopt
//! flag protocol; signedness, because the shim's types are the ones the
//! renderer chose; and wrong answers, because it compares against a
//! reference implementing the Java semantics.
//!
//! **Cannot catch:** anything about the real AMD toolchain — hiprtc's
//! acceptance of the option list, the AMDGPU backend's contraction and
//! denormal behaviour, its atomic lowering, occupancy, or performance.
//! A pass here is "this is valid C++ that computes the right thing",
//! not "this runs correctly on a Radeon".
//!
//! **Wavefront width** is the interesting case. The shim parameterises
//! it, so a kernel whose result depends on the width can be run at both
//! 32 and 64 — which is the only way to exercise AMD-specific lane
//! behaviour on a machine with no AMD part. No kernel in the rendered
//! set reads it yet (the warp-folded reduction is still `Raw`), so the
//! hook is built and the sweep is one fixture away.
//!
//! # Skipping
//!
//! Skips, loudly, when no C++ compiler is found. That is the normal
//! state on the Windows dev box and the expected state nowhere else:
//! `ubuntu-latest` has `g++` preinstalled, which is the whole reason
//! this tier is free.

use std::path::{Path, PathBuf};
use std::process::Command;

use cratonvm_jit_cuda::analyzer::{analyze, OffloadVerdict};
use cratonvm_jit_cuda::lowering::lower_method;
use cratonvm_jit_hip::{render_module, HipTarget};

/// The shim: enough of the HIP device model to run a kernel serially on
/// the host.
///
/// `warpSize` is a compile-time constant here rather than a built-in,
/// so the driver can instantiate the same kernel at 32 and at 64.
const SHIM: &str = r#"
#include <cstdio>
#include <cstdlib>
#include <cmath>

#ifndef CRATON_WAVEFRONT
#define CRATON_WAVEFRONT 32
#endif

#define __global__
#define __device__

struct Dim3 { unsigned int x, y, z; };
static Dim3 threadIdx, blockIdx, blockDim, gridDim;
static const int warpSize = CRATON_WAVEFRONT;

// One thread at a time, so a plain add is an atomic add.
template <typename T> static T atomicAdd(T* p, T v) { T old = *p; *p += v; return old; }

// Run `body` over a 1-D grid, in launch order.
//
// The loop variables are `craton_b_`/`craton_t_` rather than `b`/`t`
// because `body` expands INSIDE them: a driver whose array happens to
// be called `b` gets it shadowed by the block counter, and the failure
// surfaces as a type error a hundred lines from the cause. Not
// hypothetical -- the first version used `b`, and the first driver
// written against it duly passed an `unsigned int` where a pointer
// belonged.
#define CRATON_LAUNCH(grid, block, body)                                   \
    do {                                                                   \
        gridDim.x = (grid); gridDim.y = 1; gridDim.z = 1;                  \
        blockDim.x = (block); blockDim.y = 1; blockDim.z = 1;              \
        for (unsigned int craton_b_ = 0;                                   \
             craton_b_ < (unsigned int)(grid); ++craton_b_) {              \
            blockIdx.x = craton_b_; blockIdx.y = 0; blockIdx.z = 0;        \
            for (unsigned int craton_t_ = 0;                               \
                 craton_t_ < (unsigned int)(block); ++craton_t_) {         \
                threadIdx.x = craton_t_; threadIdx.y = 0; threadIdx.z = 0; \
                body;                                                      \
            }                                                              \
        }                                                                  \
    } while (0)
"#;

/// A host C++ compiler this machine can drive.
enum Cxx {
    /// `g++` / `clang++` / `c++`, invoked directly.
    Posix(String),
    /// MSVC, which needs its environment established first.
    ///
    /// Worth supporting rather than skipping on Windows: the dev box
    /// and the self-hosted GPU runner are both Windows, so without this
    /// arm the oracle would be written here and first RUN in CI — and a
    /// test nobody has watched pass is not evidence of anything.
    Msvc { vcvars: PathBuf },
}

/// Find a host C++ compiler, or `None`.
fn find_compiler() -> Option<Cxx> {
    for candidate in ["g++", "clang++", "c++"] {
        if Command::new(candidate)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Some(Cxx::Posix(candidate.to_string()));
        }
    }
    find_vcvars().map(|vcvars| Cxx::Msvc { vcvars })
}

/// Locate `vcvars64.bat`, the way `scripts/find-vcvars.bat` does.
///
/// `vswhere.exe` is the edition-independent answer and ships at a fixed
/// path with the Visual Studio Installer; the hardcoded fallbacks are
/// the same list that script keeps, for a machine where it is missing.
fn find_vcvars() -> Option<PathBuf> {
    let pf86 = std::env::var("ProgramFiles(x86)").ok()?;
    let vswhere = PathBuf::from(&pf86)
        .join("Microsoft Visual Studio")
        .join("Installer")
        .join("vswhere.exe");
    if vswhere.is_file() {
        if let Ok(out) = Command::new(&vswhere)
            .args([
                "-latest",
                "-products",
                "*",
                "-requires",
                "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                "-property",
                "installationPath",
            ])
            .output()
        {
            let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !root.is_empty() {
                let bat = PathBuf::from(root)
                    .join("VC")
                    .join("Auxiliary")
                    .join("Build")
                    .join("vcvars64.bat");
                if bat.is_file() {
                    return Some(bat);
                }
            }
        }
    }
    let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".to_string());
    for edition in ["Community", "Professional", "Enterprise", "BuildTools"] {
        let bat = PathBuf::from(&pf)
            .join("Microsoft Visual Studio")
            .join("2022")
            .join(edition)
            .join("VC")
            .join("Auxiliary")
            .join("Build")
            .join("vcvars64.bat");
        if bat.is_file() {
            return Some(bat);
        }
    }
    None
}

/// Lower a fixture and render it as HIP for `wavefront`.
fn render_fixture(class: &str, method: &str, descriptor: &str, wavefront: u32) -> String {
    let dir = PathBuf::from(env!("JIT_CUDA_FIXTURE_DIR"));
    let path = dir.join(format!("{class}.class"));
    let bytes =
        std::fs::read(&path).unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()));
    let mut cf = cratonvm_reader::class_reader::read_class(&bytes).expect("parse fixture");
    {
        let cp = &cf.constant_pool;
        for m in cf.methods.iter_mut() {
            for a in m.attributes.iter_mut() {
                if a.name() == "Code" {
                    let _ = a.decode(cp);
                }
            }
        }
    }
    let m = cf
        .methods
        .into_iter()
        .find(|m| &*m.name == method && &*m.descriptor == descriptor)
        .expect("fixture method");
    let OffloadVerdict::Eligible(sig) = analyze(&m) else {
        panic!("{class}.{method} is not eligible");
    };
    let module = lower_method(class, &m, &sig, 7, 5).expect("lower");
    // The arch only chooses the wavefront width and the
    // `--offload-arch` string; both RDNA and CDNA render the same body
    // today, which the sweep below is here to keep true.
    let arch = if wavefront == 64 { "gfx90a" } else { "gfx1100" };
    let target = HipTarget::from_arch(arch, true, true).expect("known arch");
    render_module(&module, &target).expect("render").source
}

/// Compile `source` and run it, returning stdout.
fn compile_and_run(cc: &Cxx, name: &str, source: &str, wavefront: u32) -> String {
    let dir = std::env::temp_dir().join(format!("cratonvm-hip-shim-{name}-{wavefront}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let src = dir.join("main.cpp");
    std::fs::write(&src, source).expect("write source");
    let exe = dir.join(if cfg!(windows) { "a.exe" } else { "a.out" });

    let out = match cc {
        Cxx::Posix(bin) => Command::new(bin)
            .arg("-std=c++17")
            // The same two rules the hiprtc option list enforces. A
            // host run that allowed contraction would be a weaker
            // oracle than the thing it is checking.
            .arg("-ffp-contract=off")
            .arg("-fno-fast-math")
            .arg("-O0")
            .arg(&src)
            .arg("-o")
            .arg(&exe)
            .output()
            .expect("invoke compiler"),
        Cxx::Msvc { vcvars } => {
            // `/fp:precise` is MSVC's spelling of "do not contract" and
            // is already the default -- named anyway, so a change to
            // that default cannot quietly weaken this oracle.
            let line = format!(
                "call \"{}\" >nul && cl /nologo /std:c++17 /EHsc /fp:precise /Od \"{}\" /Fe:\"{}\" /Fo:\"{}\\\\\"",
                vcvars.display(),
                src.display(),
                exe.display(),
                dir.display()
            );
            // `call` rather than running the batch file directly:
            // `cmd /C "\"a\" && b"` strips the outer quote pair when the
            // line both starts and ends with one, which mangles a
            // quoted path into "not recognized as an internal or
            // external command".
            // `raw_arg`, not `arg`. Rust quotes any argument
            // containing a space, so `arg(&line)` hands `cmd` a line
            // wrapped in a second pair of quotes -- and `cmd /C` then
            // applies its own outer-quote-stripping rule to the wrong
            // pair, producing "is not recognized as an internal or
            // external command" for a batch file that is plainly
            // there. `raw_arg` passes the line through untouched.
            #[cfg(windows)]
            use std::os::windows::process::CommandExt;
            let mut cmd = Command::new("cmd");
            #[cfg(windows)]
            cmd.raw_arg(format!("/C \"{line}\""));
            #[cfg(not(windows))]
            cmd.arg("/C").arg(&line);
            // `vcvars64.bat` shells out to `vswhere.exe`, which lives
            // in the installer directory and is not on PATH by default
            // -- without this it fails with "vswhere is not recognized"
            // and leaves the whole environment unset.
            if let Ok(pf86) = std::env::var("ProgramFiles(x86)") {
                let installer = format!("{pf86}\\Microsoft Visual Studio\\Installer");
                let path = std::env::var("PATH").unwrap_or_default();
                cmd.env("PATH", format!("{installer};{path}"));
            }
            cmd.output().expect("invoke cl")
        }
    };
    assert!(
        out.status.success(),
        "the emitted HIP C++ does not compile as host C++:\n{}\n{}\n--- source ---\n{source}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let run = Command::new(&exe).output().expect("run");
    assert!(
        run.status.success(),
        "the kernel crashed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    String::from_utf8_lossy(&run.stdout).into_owned()
}

/// `out[i] = a[i] + b[i]` over `n` elements, end to end.
#[test]
fn vector_add_computes_what_java_would() {
    let Some(cc) = find_compiler() else {
        eprintln!(
            "SKIP vector_add_computes_what_java_would: no g++/clang++/c++ on PATH. \
             This tier is free on ubuntu-latest and is expected to skip on Windows."
        );
        return;
    };

    for wavefront in [32u32, 64] {
        let kernel = render_fixture("EligibleVectorAdd", "vectorAdd", "([I[I[I)V", wavefront);
        let n = 1024usize;
        let block = 256usize;
        let grid = n.div_ceil(block);
        let driver = format!(
            r#"{SHIM}
{kernel}
int main() {{
    const int n = {n};
    int a[n], b[n], out[n];
    unsigned int flag = 0;
    for (int i = 0; i < n; ++i) {{ a[i] = i; b[i] = 2 * i + 1; out[i] = 0; }}
    CRATON_LAUNCH({grid}, {block},
        EligibleVectorAdd__vectorAdd____I_I_I_V(a, n, b, n, out, n, &flag, 0));
    long long checksum = 0;
    for (int i = 0; i < n; ++i) checksum += out[i];
    printf("flag=%u checksum=%lld out0=%d outlast=%d\n", flag, checksum, out[0], out[n-1]);
    return 0;
}}
"#
        );
        let stdout = compile_and_run(&cc, "vector_add", &driver, wavefront);

        // The Java answer, computed here rather than pasted: a hardcoded
        // number would still match if both sides were wrong the same way.
        let want: i64 = (0..n as i64).map(|i| i + (2 * i + 1)).sum();
        let want_last = (n as i64 - 1) + (2 * (n as i64 - 1) + 1);
        assert!(
            stdout.contains("flag=0"),
            "the kernel tripped its bounds-deopt flag on an in-range launch: {stdout}"
        );
        assert!(
            stdout.contains(&format!("checksum={want}")),
            "wavefront {wavefront}: expected checksum {want}, got: {stdout}"
        );
        assert!(
            stdout.contains(&format!("outlast={want_last}")),
            "wavefront {wavefront}: last element wrong, got: {stdout}"
        );
    }
}

/// An out-of-range array must raise the deopt flag rather than writing
/// past the end.
///
/// The bounds protocol is the one part of a kernel whose job is to
/// FAIL, so a test that only ever runs the happy path says nothing
/// about it. `p1_len` is understated, which is exactly what the
/// per-access check and the hoisted precondition exist to catch.
#[test]
fn a_short_array_raises_the_deopt_flag() {
    let Some(cc) = find_compiler() else {
        eprintln!("SKIP a_short_array_raises_the_deopt_flag: no host C++ compiler on PATH");
        return;
    };
    let kernel = render_fixture("EligibleVectorAdd", "vectorAdd", "([I[I[I)V", 32);
    let driver = format!(
        r#"{SHIM}
{kernel}
int main() {{
    const int n = 256;
    int a[n], b[n], out[n];
    unsigned int flag = 0;
    for (int i = 0; i < n; ++i) {{ a[i] = i; b[i] = i; out[i] = 0; }}
    // `b` is declared shorter than `a`, so the guard must refuse.
    CRATON_LAUNCH(1, 256,
        EligibleVectorAdd__vectorAdd____I_I_I_V(a, n, b, n / 2, out, n, &flag, 0));
    printf("flag=%u\n", flag);
    return 0;
}}
"#
    );
    let stdout = compile_and_run(&cc, "short_array", &driver, 32);
    assert!(
        stdout.contains("flag=1"),
        "a short input array must raise the deopt flag so the host reruns on \
         the CPU; got: {stdout}"
    );
}

/// Every rendered golden must at least COMPILE as host C++.
///
/// The per-kernel correctness tests above cover one shape carefully;
/// this covers all of them shallowly, which is the pairing that catches
/// a renderer change that breaks a kernel nobody wrote a driver for.
#[test]
fn every_rendered_golden_compiles() {
    let Some(cc) = find_compiler() else {
        eprintln!("SKIP every_rendered_golden_compiles: no host C++ compiler on PATH");
        return;
    };
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("hip");
    let mut checked = 0usize;
    let entries = std::fs::read_dir(&dir).expect("golden dir; record it first");
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|e| e == "hip") {
            continue;
        }
        let kernel = std::fs::read_to_string(&path).expect("read golden");
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        let driver = format!("{SHIM}\n{kernel}\nint main() {{ return 0; }}\n");
        // Reuse the compile half; the run is a no-op `main`.
        compile_and_run(&cc, &format!("compile-{stem}"), &driver, 32);
        checked += 1;
    }
    assert!(
        checked > 0,
        "no rendered goldens found in {}; record them with \
         `GPU_GOLDEN_BLESS=1 cargo test -p cratonvm-jit-hip`",
        dir.display()
    );
    eprintln!("every_rendered_golden_compiles: {checked} kernels compiled as host C++");
}
