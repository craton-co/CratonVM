#!/usr/bin/env bash
# Reproducibility evidence collector (bash half; scripts/evidence/collect.ps1 is
# the PowerShell twin and writes the same file names with the same section
# headers).
#
# WHY THIS EXISTS
# ---------------
# Every performance number, bug report, differential-gate verdict and "it works
# on my machine" in this repository is a claim about a *specific* tree, a
# *specific* toolchain and a *specific* CPU. Without a manifest, none of them
# can be re-run six months later: the report lane "Reproducibility" exists
# precisely because the repository could not, from a result alone, say which
# commit / rustc / JDK / CPU produced it.
#
# This script writes that manifest into `evidence/`:
#
#   evidence/source.txt            tree identity: UTC timestamp, HEAD, worktree
#                                  cleanliness, submodules, last 30 commits and
#                                  the history of the five subsystems whose
#                                  churn most often invalidates a result
#                                  (jit/, gc/, vm/src/runtime, types/src/value.rs,
#                                  classloading/).
#   evidence/environment.txt       toolchain + machine identity: OS, CPU model,
#                                  memory, rustc -Vv, cargo -V, java/javac, cc,
#                                  and the CPU feature bits (the .cargo/config
#                                  baseline is +sse4.2,+pclmulqdq, so the host's
#                                  actual feature set is load-bearing).
#   evidence/cargo-metadata.json   machine-readable workspace + resolved deps.
#   evidence/cargo-tree.txt        human-readable dependency tree.
#   evidence/cargo-duplicates.txt  `cargo tree --duplicates` (the workspace
#                                  Cargo.toml documents three accepted duplicate
#                                  families; this is how a fourth gets noticed).
#   evidence/cargo-features.txt    every feature every workspace member declares,
#                                  plus the default sets. Non-default features
#                                  are how this repository once lost 1,522 tests
#                                  to a configuration nothing compiled.
#
# USAGE
#   scripts/evidence/collect.sh                 # -> ./evidence
#   scripts/evidence/collect.sh out/manifest    # -> ./out/manifest
#   EVIDENCE_DIR=/tmp/e scripts/evidence/collect.sh
#
# Runnable from anywhere: it relocates to the repository root itself.
#
# EXIT STATUS
#   0  manifest written (possibly with "not available" entries -- a missing
#      optional tool is recorded, never fatal).
#   2  could not locate the repository root, or could not create the output
#      directory. Nothing was written.
#
# `set -e` is deliberately NOT used. Half of what this collects is optional
# (javac on a machine with only a JRE, `cc` on a bare Windows box, `git
# submodule` in a tarball export). A collector that aborts on the first absent
# tool produces no manifest at all, which is strictly worse than a manifest that
# says "java: not found".

set -uo pipefail

# --------------------------------------------------------------------------
# Locate the repository root and relocate to it.
# --------------------------------------------------------------------------
script_path=${BASH_SOURCE[0]//\\//}
script_dir=$(cd -- "$(dirname -- "$script_path")" && pwd) || exit 2

if repo_root=$(git -C "$script_dir" rev-parse --show-toplevel 2>/dev/null); then
    :
else
    # Not a git checkout (tarball export, vendored copy). Fall back to the
    # script's own location: scripts/evidence/collect.sh -> ../..
    repo_root=$(cd -- "$script_dir/../.." && pwd) || exit 2
fi

cd -- "$repo_root" || exit 2

out_dir=${1:-${EVIDENCE_DIR:-evidence}}
mkdir -p -- "$out_dir" || {
    echo "error: cannot create output directory: $out_dir" >&2
    exit 2
}
# Absolute, so the `cd`-free rest of the script is unambiguous.
out_dir=$(cd -- "$out_dir" && pwd)

SOURCE_TXT="$out_dir/source.txt"
ENV_TXT="$out_dir/environment.txt"

have() { command -v "$1" >/dev/null 2>&1; }

# Print a section banner. Keeping the shape identical to collect.ps1 means a
# Linux manifest and a Windows manifest diff cleanly against each other.
section() {
    printf '\n===== %s =====\n' "$1"
}

# Run a command, or record why it could not run. Never fails the script.
# usage: capture <label> <cmd> [args...]
capture() {
    local label=$1
    shift
    section "$label"
    if have "$1"; then
        "$@" 2>&1 || printf '(command exited %s)\n' "$?"
    else
        printf '(not available: %s is not on PATH)\n' "$1"
    fi
}

# --------------------------------------------------------------------------
# evidence/source.txt -- what tree produced this result
# --------------------------------------------------------------------------
{
    printf 'CratonVM evidence manifest: SOURCE\n'
    printf 'generated (UTC): %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || echo unknown)"
    printf 'generated by:    scripts/evidence/collect.sh\n'
    printf 'repository root: %s\n' "$repo_root"

    section 'git rev-parse HEAD'
    git rev-parse HEAD 2>&1 || printf '(no git metadata)\n'

    section 'git rev-parse --abbrev-ref HEAD'
    git rev-parse --abbrev-ref HEAD 2>&1 || printf '(no git metadata)\n'

    section 'git describe --tags --always --dirty'
    git describe --tags --always --dirty 2>&1 || printf '(no tags reachable)\n'

    # The single most important line in the manifest. A dirty worktree means the
    # result is NOT reproducible from the recorded commit, and every downstream
    # consumer of this manifest must treat it as provisional.
    section 'git status --short --branch'
    git status --short --branch 2>&1 || printf '(no git metadata)\n'

    section 'worktree cleanliness'
    if git diff --quiet 2>/dev/null && git diff --cached --quiet 2>/dev/null; then
        printf 'CLEAN: tracked files match HEAD.\n'
    else
        printf 'DIRTY: tracked files differ from HEAD -- results are NOT\n'
        printf 'reproducible from the commit recorded above. Diffstat:\n'
        git diff --stat HEAD 2>&1
    fi

    section 'git submodule status'
    # The repository currently has no .gitmodules; this stays so the manifest
    # keeps its shape if one is added, rather than silently omitting a
    # submodule pin that a result depends on.
    if [ -f .gitmodules ]; then
        git submodule status --recursive 2>&1
    else
        printf '(no .gitmodules in this tree)\n'
    fi

    section 'last 30 commits'
    git log -n 30 --date=iso-strict \
        --pretty=format:'%h %ad %an %d %s' 2>&1
    printf '\n'

    # Subsystem churn. These five paths are where a change silently invalidates
    # a previously recorded benchmark, GC audit or differential verdict, so the
    # manifest carries their recent history separately from the trunk log.
    section 'recent history: jit/ gc/ vm/src/runtime types/src/value.rs classloading/'
    git log -n 30 --date=iso-strict \
        --pretty=format:'%h %ad %an %s' \
        -- jit gc vm/src/runtime types/src/value.rs classloading 2>&1
    printf '\n'

    section 'per-subsystem commit counts (last 200 commits)'
    for p in jit gc vm/src/runtime types/src/value.rs classloading; do
        n=$(git log -n 200 --oneline -- "$p" 2>/dev/null | wc -l | tr -d ' ')
        last=$(git log -n 1 --date=iso-strict --pretty=format:'%h %ad' -- "$p" 2>/dev/null)
        printf '%-24s commits=%-4s last=%s\n' "$p" "${n:-0}" "${last:-none}"
    done
} >"$SOURCE_TXT" 2>&1

# --------------------------------------------------------------------------
# evidence/environment.txt -- what machine and toolchain produced this result
# --------------------------------------------------------------------------
{
    printf 'CratonVM evidence manifest: ENVIRONMENT\n'
    printf 'generated (UTC): %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || echo unknown)"

    section 'operating system'
    if have uname; then
        uname -a 2>&1
    fi
    if [ -r /etc/os-release ]; then
        cat /etc/os-release 2>&1
    elif have sw_vers; then
        sw_vers 2>&1
    elif [ -n "${OS:-}" ]; then
        printf 'OS=%s\n' "$OS"
    fi

    section 'CPU'
    if [ -r /proc/cpuinfo ]; then
        # model name + core count, not the whole 4 KB per-core dump.
        grep -m 1 -E '^model name' /proc/cpuinfo 2>/dev/null || true
        printf 'logical cpus: %s\n' "$(grep -c -E '^processor' /proc/cpuinfo 2>/dev/null || echo unknown)"
    elif have sysctl; then
        sysctl -n machdep.cpu.brand_string 2>/dev/null || true
        printf 'logical cpus: %s\n' "$(sysctl -n hw.logicalcpu 2>/dev/null || echo unknown)"
    else
        printf '(no /proc/cpuinfo and no sysctl)\n'
    fi

    section 'CPU features'
    # The workspace pins `-C target-feature=+sse4.2,+pclmulqdq` in
    # .cargo/config.toml. A host that lacks them produces a binary that will not
    # run; a host that has AVX-512 produces different auto-vectorisation. Both
    # change results, so the actual feature set is part of the evidence.
    if [ -r /proc/cpuinfo ]; then
        grep -m 1 -E '^flags|^Features' /proc/cpuinfo 2>/dev/null || printf '(no flags line)\n'
    elif have sysctl; then
        sysctl -n machdep.cpu.features 2>/dev/null || true
        sysctl -n machdep.cpu.leaf7_features 2>/dev/null || true
        sysctl -n hw.optional.arm64 2>/dev/null && printf '(arm64 host)\n'
    else
        printf '(not available on this platform via bash; see collect.ps1)\n'
    fi

    section 'memory'
    if [ -r /proc/meminfo ]; then
        grep -E '^(MemTotal|MemAvailable|SwapTotal)' /proc/meminfo 2>/dev/null
    elif have sysctl; then
        printf 'hw.memsize: %s\n' "$(sysctl -n hw.memsize 2>/dev/null || echo unknown)"
    else
        printf '(not available)\n'
    fi

    # `rustc -Vv` carries the commit hash and the host triple -- the two facts
    # that make a codegen difference explainable.
    capture 'rustc -Vv' rustc -Vv
    capture 'cargo -V' cargo -V
    capture 'rustup show (active toolchain)' rustup show

    section 'java -version'
    if have java; then
        java -version 2>&1
    else
        printf '(not available: java is not on PATH)\n'
    fi
    printf 'JAVA_HOME=%s\n' "${JAVA_HOME:-<unset>}"

    section 'javac -version'
    if have javac; then
        javac -version 2>&1
    else
        printf '(not available: javac is not on PATH -- build.rs steps that\n'
        printf 'compile Java fixtures will skip, and several test targets will\n'
        printf 'report as skipped rather than failed)\n'
    fi

    capture 'cc --version' cc --version
    capture 'gcc --version' gcc --version
    capture 'clang --version' clang --version
    capture 'ld --version' ld --version

    section 'relevant environment variables'
    # Only the ones that change what gets built or how the VM behaves. Values,
    # not just names: `CRATONVM_JIT=off` and `CRATONVM_JIT=on` are different runs.
    env 2>/dev/null \
        | grep -E '^(CRATONVM_|RUSTFLAGS=|RUSTDOCFLAGS=|CARGO_|RUST_BACKTRACE=|JAVA_HOME=|KRUN_)' \
        | sort \
        || printf '(none set)\n'
} >"$ENV_TXT" 2>&1

# --------------------------------------------------------------------------
# cargo-derived evidence
# --------------------------------------------------------------------------
if have cargo; then
    # `--no-deps` would drop the resolved third-party graph, which is the half
    # that actually varies between machines. Keep the full resolve.
    cargo metadata --format-version 1 >"$out_dir/cargo-metadata.json" 2>"$out_dir/cargo-metadata.err.txt" \
        || printf '(cargo metadata failed; see cargo-metadata.err.txt)\n' >"$out_dir/cargo-metadata.json"
    [ -s "$out_dir/cargo-metadata.err.txt" ] || rm -f "$out_dir/cargo-metadata.err.txt"

    cargo tree --workspace --edges normal,build >"$out_dir/cargo-tree.txt" 2>&1 \
        || cargo tree >"$out_dir/cargo-tree.txt" 2>&1 \
        || printf '(cargo tree failed)\n' >"$out_dir/cargo-tree.txt"

    {
        printf 'cargo tree --duplicates --workspace\n'
        printf '\n'
        printf 'The workspace Cargo.toml documents three ACCEPTED duplicate families\n'
        printf '(hashbrown, getrandom, windows-sys) with the reason each pair cannot be\n'
        printf 'unified. Anything outside those three is new and should be triaged.\n'
        printf '\n'
        cargo tree --duplicates --workspace 2>&1 || cargo tree --duplicates 2>&1
    } >"$out_dir/cargo-duplicates.txt"

    # Feature inventory. Prefer python3 over jq: tools/check_markdown_links.py
    # already makes python3 a de-facto repo prerequisite, whereas jq is not
    # installed on a default Windows dev box.
    # NOTE: `command -v python3` is NOT sufficient on Windows. A default Windows
    # install ships an App Execution Alias at
    # %LOCALAPPDATA%\Microsoft\WindowsApps\python3.exe that exists on PATH,
    # resolves fine, and then prints "Python was not found; run without
    # arguments to install from the Microsoft Store" for every invocation. So
    # each candidate is probed by actually running it.
    py=
    for cand in python3 python py; do
        if have "$cand" && "$cand" -c 'import json,sys' >/dev/null 2>&1; then
            py=$cand
            break
        fi
    done

    if [ -n "$py" ]; then
        "$py" - "$out_dir/cargo-metadata.json" >"$out_dir/cargo-features.txt" 2>&1 <<'PYEOF'
import json, sys

path = sys.argv[1]
try:
    with open(path, "r", encoding="utf-8") as fh:
        md = json.load(fh)
except Exception as exc:  # noqa: BLE001 - manifest must never abort
    print("(could not read cargo-metadata.json: %s)" % exc)
    sys.exit(0)

members = set(md.get("workspace_members", []))
pkgs = [p for p in md.get("packages", []) if p.get("id") in members]
pkgs.sort(key=lambda p: p.get("name", ""))

print("CratonVM workspace feature inventory")
print("")
print("Every feature below is a distinct compile configuration. Features that")
print("are NOT in a crate's `default` set are compiled only by the jobs in")
print(".github/workflows/feature-matrix.yml (and the feature-gate jobs in")
print("ci.yml). A feature compiled by no job rots: this repository lost 1,522")
print("tests that way once already.")
print("")
print("%-36s %s" % ("PACKAGE", "DEFAULT FEATURES"))
for p in pkgs:
    feats = p.get("features", {}) or {}
    print("%-36s %s" % (p.get("name", "?"), ", ".join(sorted(feats.get("default", []))) or "(none)"))

print("")
print("PER-PACKAGE FEATURE TABLE (package/feature -> enables)")
for p in pkgs:
    feats = p.get("features", {}) or {}
    name = p.get("name", "?")
    print("")
    print("[%s]  %s" % (name, p.get("manifest_path", "")))
    if not feats:
        print("  (declares no [features] table)")
        continue
    for f in sorted(feats):
        enables = feats[f]
        print("  %-32s -> %s" % (f, ", ".join(enables) if enables else "(leaf)"))

qualified = []
for p in pkgs:
    name = p.get("name", "?")
    for f in sorted((p.get("features", {}) or {})):
        if f == "default":
            continue
        qualified.append("%s/%s" % (name, f))

print("")
print("QUALIFIED NON-DEFAULT FEATURE LIST (%d entries)" % len(qualified))
print("This is exactly the list feature-matrix.yml enumerates, minus the")
print("gpu/cuda entries it routes to cuda-bridge.yml and gpu-selfhosted.yml.")
for q in qualified:
    print("  " + q)
PYEOF
    else
        printf '(no working python3/python/py interpreter found; the feature\n' >"$out_dir/cargo-features.txt"
        printf 'inventory needs one to parse cargo-metadata.json. On Windows,\n' >>"$out_dir/cargo-features.txt"
        printf 'run scripts/evidence/collect.ps1 instead -- it builds the same\n' >>"$out_dir/cargo-features.txt"
        printf 'table in PowerShell with no interpreter prerequisite. Otherwise\n' >>"$out_dir/cargo-features.txt"
        printf 'read cargo-metadata.json directly: `.packages[].features` is the\n' >>"$out_dir/cargo-features.txt"
        printf 'same data.)\n' >>"$out_dir/cargo-features.txt"
    fi
else
    for f in cargo-metadata.json cargo-tree.txt cargo-duplicates.txt cargo-features.txt; do
        printf '(not available: cargo is not on PATH)\n' >"$out_dir/$f"
    done
fi

printf 'evidence written to: %s\n' "$out_dir"
ls -l -- "$out_dir" 2>/dev/null || true
