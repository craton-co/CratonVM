// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 25, lane L6: what a retransform of a class whose
// small method some compile SPLICED costs the unrelated hot compiled code of
// the rest of the program (stage 1-3 of
// docs/internal/fixed-bugs/interpreter-L1-proposal-per-body-copied-bytecode-dependencies-FIXED-20260930.md).
//
// `Victim.value` is spliced into `Spliced.run`'s loop (both tiers splice a
// one-line static). `Hot.run` never touches Victim. Each round retransforms
// Victim (no transformer is registered, so its bytes do not change -- a
// coverage agent or Mockito retransforms like this) and then IMMEDIATELY
// times a slice of `Hot.run`, with no re-warm in between.
//
//   * Up to wave 24 (and in wave 25 with the default
//     `SCOPE_REDEFINITIONS_BY_COPIED_CLASSES = false`, jit/src/lib.rs): some
//     compile copied Victim's bytecode, so every retransform takes the
//     WHOLE-CACHE path -- every published body is withdrawn and made not
//     entrant, `Hot.run` included, and runs interpreted until it recompiles:
//     after/before well above 1. With JIT on and CRATONVM_DBG_JITC=1 each
//     retransform prints `[cratonvm-jitc] not-entrant pass: candidates=N`
//     with N = every published body (the wave-24 host saw 7 for one test
//     class in a small probe; a warm application has hundreds), and since
//     wave 25 `[cratonvm-jitc] exit polls forced: bodies=...`.
//   * Wave 25 with the const flipped to `true` (a rebuilt binary): the copy is
//     recorded on the bodies whose compile made it
//     (`CompiledMethod::copied_classes`), the redefinition stays SCOPED, and
//     only Spliced's bodies (and Victim's own) are withdrawn: `candidates=`
//     drops to those (1-3) and after/before approaches 1.
//
// Rows to compare (JIT on, --compatible and the default, 5 runs, medians):
// stderr `after/before=` between the const-false and const-true builds
// (expected: lower with true), and the `candidates=` count of the
// `not-entrant pass:` lines under CRATONVM_DBG_JITC=1 (expected: from
// "every body" to "1-3"). `--nojit` prints the same checksum and times
// nothing of interest.
//
// SETUP: an agent jar whose manifest has
//     Premain-Class: RedefineSplicedScopeBench$Agent
//     Can-Retransform-Classes: true
// containing RedefineSplicedScopeBench*.class, then
//     java|cratonvm [--compatible] -javaagent:bench.jar -cp bench.jar RedefineSplicedScopeBench [rounds]
// Without the agent the retransforms are skipped and stderr says so.
//
// stdout is deterministic: one "checksum=" line. HotSpot 25 (agent, 40
// rounds, JIT on; -Xint without the agent prints the same line) prints
//     checksum=6031495897943536768
// and on stderr (i7-8550U, two runs) before=2.72 / 2.48 ns/iter,
// after=2.50 / 2.76, after/before=0.92 / 1.11: HotSpot keeps every nmethod
// that does not depend on Victim (it records the inlining as a dependency).
import java.lang.instrument.Instrumentation;

public class RedefineSplicedScopeBench {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    static final class Victim {
        static int value(int x) {
            return x + 7;
        }
    }

    static final class Spliced {
        static long run(int n) {
            long s = 0;
            for (int i = 0; i < n; i++) {
                s += Victim.value(i);
            }
            return s;
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

    static final int SLICE = 200_000;

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        long checksum = 0;
        for (int w = 0; w < 200; w++) {
            checksum += Hot.run(SLICE);
            checksum += Spliced.run(SLICE);
        }

        long before = 0;
        for (int r = 0; r < rounds; r++) {
            long t0 = System.nanoTime();
            checksum += Hot.run(SLICE);
            before += System.nanoTime() - t0;
        }

        Instrumentation i = inst;
        boolean retransform = i != null && i.isRetransformClassesSupported()
                && i.isModifiableClass(Victim.class);
        if (!retransform) {
            System.err.println("no agent: the retransforms were skipped (see the header)");
        }
        long after = 0;
        for (int r = 0; r < rounds; r++) {
            if (retransform) {
                i.retransformClasses(Victim.class);
            }
            long t0 = System.nanoTime();
            checksum += Hot.run(SLICE);
            after += System.nanoTime() - t0;
            // Keeps the splice compiled (and so the next retransform's
            // dependency) between rounds.
            checksum += Spliced.run(SLICE);
        }

        System.out.println("checksum=" + checksum);
        double perBefore = (double) before / ((long) rounds * SLICE);
        double perAfter = (double) after / ((long) rounds * SLICE);
        System.err.printf("before=%.2f ns/iter after=%.2f ns/iter after/before=%.2f%n",
                perBefore, perAfter, perAfter / perBefore);
    }
}
