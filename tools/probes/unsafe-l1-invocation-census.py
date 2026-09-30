#!/usr/bin/env python3
"""Union `invocations` for the seven retirement candidates across the corpus.

An absence from all three JDK images says nothing can NAME these methods. This
says nothing DID. The campaign's rule is that a retirement needs both, plus a
positive control -- a zero from an instrument that never ran is not a zero
(`G33-1`, and `H11-3`'s `DataInputStream.readInt` at 540 in the same runs).

Three things this is careful about:

  * the dump flag must come BEFORE the main class or it is silently ignored --
    no file, no warning, exit 0;
  * the report is NOT written when the program calls `System.exit`, so a vector
    with no dump is a BLIND row, never a zero. Counted separately;
  * dumps are ~7 MB each; parsed and deleted per vector so 226 of them do not
    fill /data.
"""
import json, os, subprocess, sys, collections

W = "/data/cvm-l1u-20260828"
CV = "/data/l1u-target/release/cratonvm"
JDK = "/data/toolchain/jdk-25"
BUILD = W + "/regression-suite/build"
CP = ":".join([BUILD,
               W + "/regression-suite/build-modules/cratonvm.jdkonly.svc",
               W + "/regression-suite/resources"])
DUMP = "/data/l1u-inv/dump.json"

# KEYED ON THE FULL TRIPLE. Keyed on (class, name) the first run reported
# `park` at 837 invocations in 14 vectors -- because `jdk.internal.misc.Unsafe`
# registers TWO `park` overloads and the LIVE one, `park(ZJ)V`, was being summed
# into the candidate's row. A bare name is not a triple.
TARGETS = [
    ("jdk/internal/misc/Unsafe", "defineAnonymousClass",
     "(Ljava/lang/Class;[B[Ljava/lang/Object;)Ljava/lang/Class;"),
    ("jdk/internal/misc/Unsafe", "getReferencePlain", "(Ljava/lang/Object;J)Ljava/lang/Object;"),
    ("jdk/internal/misc/Unsafe", "putReferencePlain", "(Ljava/lang/Object;JLjava/lang/Object;)V"),
    ("jdk/internal/misc/Unsafe", "monitorEnter", "(Ljava/lang/Object;)V"),
    ("jdk/internal/misc/Unsafe", "monitorExit", "(Ljava/lang/Object;)V"),
    ("jdk/internal/misc/Unsafe", "park", "(Ljava/lang/Object;J)V"),
    ("sun/misc/Unsafe", "defineClass",
     "(Ljava/lang/String;[BIILjava/lang/ClassLoader;Ljava/security/ProtectionDomain;)Ljava/lang/Class;"),
]
# Must be NON-ZERO, or the zeros above are about the harness, not the methods.
CONTROLS = [
    ("jdk/internal/misc/Unsafe", "objectFieldOffset1", "(Ljava/lang/Class;Ljava/lang/String;)J"),
    ("jdk/internal/misc/Unsafe", "compareAndSetInt", "(Ljava/lang/Object;JII)Z"),
    ("jdk/internal/misc/Unsafe", "arrayIndexScale", "(Ljava/lang/Class;)I"),
    # The LIVE park overload -- the positive control that makes the candidate
    # overload's zero mean something, since they differ only by descriptor.
    ("jdk/internal/misc/Unsafe", "park", "(ZJ)V"),
]

def vectors():
    out = []
    for f in sorted(os.listdir(BUILD)):
        if f.endswith(".class") and "$" not in f:
            out.append(f[:-6])
    return out

def run(mode, cls):
    os.makedirs("/data/l1u-inv", exist_ok=True)
    for p in (DUMP,):
        if os.path.exists(p):
            os.remove(p)
    cmd = [CV, "--java-home", JDK]
    if mode == "strict":
        cmd.append("--jdk-only")
    # BEFORE -cp, deliberately.
    cmd += ["--dump-native-registry", DUMP, "-cp", CP, cls]
    env = dict(os.environ, CRATONVM_DISABLE_DEFAULT_WATCHDOG="1")
    try:
        subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                       timeout=120, env=env)
    except subprocess.TimeoutExpired:
        pass
    if not os.path.exists(DUMP):
        return None
    try:
        with open(DUMP, encoding="utf-8") as fh:
            d = json.load(fh)
    except Exception:
        return None
    finally:
        try: os.remove(DUMP)
        except OSError: pass
    acc = collections.Counter()
    incomplete = set()
    for r in d.get("natives", []):
        key = (r.get("class"), r.get("name"), r.get("descriptor"))
        if key in TARGETS or key in CONTROLS:
            acc[key] += r.get("invocations", 0) or 0
            if not r.get("invocations_complete", True):
                incomplete.add(key)
    return acc, incomplete

def main():
    modes = sys.argv[1:] or ["compat", "strict"]
    vs = vectors()
    print("vectors:", len(vs))
    for mode in modes:
        total = collections.Counter()
        hit_vectors = collections.Counter()
        blind = []
        incomplete_any = set()
        for i, c in enumerate(vs, 1):
            got = run(mode, c)
            if got is None:
                blind.append(c)
                continue
            acc, inc = got
            incomplete_any |= inc
            for k, v in acc.items():
                total[k] += v
                if v:
                    hit_vectors[k] += 1
        print("--- %s: %d vectors, %d produced a dump, %d BLIND (no report written)"
              % (mode, len(vs), len(vs) - len(blind), len(blind)))
        print("    CONTROLS (must be non-zero):")
        for k in CONTROLS:
            print("      %-34s %-26s %7d inv in %3d vectors"
                  % (k[0].split("/")[-2] + "/Unsafe." + k[1], k[2][:26], total[k], hit_vectors[k]))
        print("    RETIREMENT CANDIDATES:")
        for k in TARGETS:
            print("      %-34s %-26s %7d inv in %3d vectors"
                  % (k[0].split("/")[-2] + "/Unsafe." + k[1], k[2][:26], total[k], hit_vectors[k]))
        if incomplete_any:
            print("    !! invocations_complete=false for:", sorted(incomplete_any))
        if blind:
            print("    blind vectors:", " ".join(blind[:12]),
                  "..." if len(blind) > 12 else "")
    print("INV-DONE")

main()
