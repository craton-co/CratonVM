// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 117, lane L3 (proposal
// `i14-L3-proposal-class-scoped-redefinition-invalidation`): two switches, both
// default off, both performance:
//   CRATONVM_REDEFINE_IDENTICAL_FAST_PATH            a byte-identical retransform
//                                                    installs nothing (wave-28 note);
//   CRATONVM_JIT_SCOPED_REDEFINITION_KEEPS_CODE_STATE a scoped redefinition does not
//                                                    advance the code-state epoch (stage 3).
//
// Compared lines (deterministic, HotSpot 25 below):
//   * `toggle:` a retransform-capable transformer rewrites `Toggle.v()`'s literal to
//     the value `mode` names; the row retransforms with mode 1, 1 again (an identical
//     file: the fast path's case), 2, 2, 1, and prints what `v()` answers after each.
//     A fast path that skipped a real change would print a stale value here.
//   * `checksum=` of the timed loops.
// Timing lines (`TIME`, not compared): ns per iteration of a hot loop timed right
// after each retransform, against the same loop before any:
//   * `unrelated`: retransforms of `Victim` (nothing inlines it): stage 3's row;
//   * `inlined`: retransforms of `Callee`, which `Hot2.run` inlines: the identical
//     fast path's row (without it each retransform withdraws `Hot2.run`).
// Expected: with the matching switch, after/before falls toward 1. Compare the four
// arms (none, each switch, both), JIT on, interleaved, medians; `--nojit` only checks
// the compared lines. `CRATONVM_DBG_RETRANSFORM=1` prints
// "[redefine] identical retransform of ...: nothing installed" under the fast path.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W117RetransformIdenticalBench$Agent
//     Can-Retransform-Classes: true
// containing L3W117RetransformIdenticalBench*.class, then
//     java|cratonvm [--jdk-only|--compatible] [--nojit] -javaagent:bench.jar -cp bench.jar L3W117RetransformIdenticalBench
// Without the agent it prints "no agent". (No HOTSPOT_EXPECTED block: the battery runs
// no agent.) HotSpot 25.0.3 prints, with the agent:
//     toggle: v001 v001 v002 v002 v001
//     checksum=-5344440173129573497   (default 40 rounds)
// HotSpot itself pays on the `inlined` row (after/before ~22 on Windows: it deoptimizes
// `Hot2.run` on every retransform of `Callee`, identical or not); `unrelated` ~1.
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;

public class L3W117RetransformIdenticalBench {
    static volatile Instrumentation inst;
    static volatile int mode;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Toggle {
        public static String v() {
            return "v000";
        }
    }

    static final class Victim {
        int seed = 7;

        int v() {
            return seed;
        }
    }

    static final class Callee {
        int seed = 3;

        int add(int x) {
            return x + seed;
        }
    }

    static final class Hot {
        static int mix(int x) {
            return (x * 31) ^ (x >>> 3);
        }

        static long run(int n) {
            long s = 0;
            for (int i = 0; i < n; i++) {
                s += mix(i);
                s ^= s << 1;
            }
            return s;
        }
    }

    static final class Hot2 {
        static long run(Callee c, int n) {
            long s = 0;
            for (int i = 0; i < n; i++) {
                s += c.add(i);
                s ^= s << 1;
            }
            return s;
        }
    }

    /** Rewrites `Toggle`'s literal `v000` to `v00<mode>`; every other class unchanged. */
    static final class ToggleTransformer implements ClassFileTransformer {
        @Override
        public byte[] transform(Module module, ClassLoader loader, String name, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (!"L3W117RetransformIdenticalBench$Toggle".equals(name) || mode == 0) {
                return null;
            }
            byte[] b = bytes.clone();
            for (int i = 0; i + 4 <= b.length; i++) {
                if (b[i] == 'v' && b[i + 1] == '0' && b[i + 2] == '0' && b[i + 3] == '0') {
                    b[i + 3] = (byte) ('0' + mode);
                }
            }
            return b;
        }
    }

    static final int SLICE = 200_000;

    static double[] timed(Instrumentation i, Class<?> target, int rounds, java.util.function.LongSupplier body,
            long[] checksum) throws Exception {
        long before = 0;
        for (int r = 0; r < rounds; r++) {
            long t0 = System.nanoTime();
            checksum[0] += body.getAsLong();
            before += System.nanoTime() - t0;
        }
        long after = 0;
        for (int r = 0; r < rounds; r++) {
            i.retransformClasses(target);
            long t0 = System.nanoTime();
            checksum[0] += body.getAsLong();
            after += System.nanoTime() - t0;
        }
        return new double[] {(double) before / ((long) rounds * SLICE), (double) after / ((long) rounds * SLICE)};
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        i.addTransformer(new ToggleTransformer(), true);

        StringBuilder toggle = new StringBuilder("toggle:");
        Toggle.v();
        for (int m : new int[] {1, 1, 2, 2, 1}) {
            mode = m;
            i.retransformClasses(Toggle.class);
            toggle.append(' ').append(Toggle.v());
        }
        System.out.println(toggle);

        long[] checksum = {0};
        Callee callee = new Callee();
        checksum[0] += new Victim().v();
        for (int w = 0; w < 200; w++) {
            checksum[0] += Hot.run(SLICE);
            checksum[0] += Hot2.run(callee, SLICE);
        }
        double[] unrelated = timed(i, Victim.class, rounds, () -> Hot.run(SLICE), checksum);
        double[] inlined = timed(i, Callee.class, rounds, () -> Hot2.run(callee, SLICE), checksum);
        System.out.println("checksum=" + checksum[0]);
        System.out.printf(java.util.Locale.ROOT, "TIME unrelated before=%.2f after=%.2f ns/iter after/before=%.2f%n",
                unrelated[0], unrelated[1], unrelated[1] / unrelated[0]);
        System.out.printf(java.util.Locale.ROOT, "TIME inlined before=%.2f after=%.2f ns/iter after/before=%.2f%n",
                inlined[0], inlined[1], inlined[1] / inlined[0]);
    }
}
