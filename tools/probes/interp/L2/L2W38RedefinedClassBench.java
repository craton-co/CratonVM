// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 38, lane L2: timing companion of
// L2W38RedefinedClassCompiles (item 5 of the wave-37 compile-door review).
// A timing probe: its numbers are not compared with HotSpot's.
//
// `Target` is redefined with its OWN bytes (what a retransforming agent that
// decides not to weave leaves behind: the class's redefinition generation
// moves, its code does not), and three shapes are timed before and after,
// each as ns per operation, best of five rounds:
//
//   loop     a hot loop inside Target (`Target.loop(n)`), entered once per
//            round: OSR. Default mode builds OSR bodies on the background
//            worker only, which declined a redefined class's OSR tasks in
//            waves 38-39 (this row ran interpreted after the redefinition);
//            since wave 40 it compiles them (interpreter-L2-an-obsolete-
//            activation-enters-an-osr-body-of-the-new-bytecode-FIXED), so
//            the `loop after` row is the one wave 40 moves.
//   callee   `Caller.drive(n)` (never redefined) calls `Target.add(x)` n
//            times from its compiled body (callee door, then the worker).
//   site     this class's own loop calls `Target.add(x)`: an OSR'd loop of
//            a class never redefined, so a compiled caller too.
//
// SETUP: as L2W38RedefinedClassCompiles (Premain-Class
// L2W38RedefinedClassBench$Agent, Can-Redefine-Classes: true), then
//     cratonvm [--compatible] -javaagent:bench.jar -cp bench.jar L2W38RedefinedClassBench
//     CRATONVM_JIT_BG_DECLINE_REDEFINED=1 cratonvm -javaagent:bench.jar -cp bench.jar L2W38RedefinedClassBench
// The second line restores the old worker decline (method entry too): the
// `callee after` row is the one wave 38 moves by default. Prints one line per shape and phase:
//     <shape> before=<ns/op> after=<ns/op> check=<checksum>
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;

public class L2W38RedefinedClassBench {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Target {
        public static int add(int x) {
            return x * 31 + 7;
        }

        public static int loop(int n) {
            int acc = 0;
            for (int i = 0; i < n; i++) {
                acc = acc * 31 + i;
            }
            return acc;
        }
    }

    public static class Caller {
        public static int drive(int n) {
            int acc = 0;
            for (int i = 0; i < n; i++) {
                acc += Target.add(i);
            }
            return acc;
        }
    }

    static int sink;

    static long bestLoop(int n) {
        long best = Long.MAX_VALUE;
        for (int r = 0; r < 5; r++) {
            long t = System.nanoTime();
            sink += Target.loop(n);
            best = Math.min(best, System.nanoTime() - t);
        }
        return best;
    }

    static long bestCallee(int n) {
        long best = Long.MAX_VALUE;
        for (int r = 0; r < 5; r++) {
            long t = System.nanoTime();
            sink += Caller.drive(n);
            best = Math.min(best, System.nanoTime() - t);
        }
        return best;
    }

    static long bestSite(int n) {
        long best = Long.MAX_VALUE;
        for (int r = 0; r < 5; r++) {
            long t = System.nanoTime();
            int acc = 0;
            for (int i = 0; i < n; i++) {
                acc += Target.add(i);
            }
            sink += acc;
            best = Math.min(best, System.nanoTime() - t);
        }
        return best;
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        byte[] own;
        try (InputStream in = L2W38RedefinedClassBench.class
                .getResourceAsStream("L2W38RedefinedClassBench$Target.class")) {
            own = in.readAllBytes();
        }
        int n = 2_000_000;
        // Warm every shape so each is compiled before the first timed round.
        for (int w = 0; w < 3; w++) {
            bestLoop(n);
            bestCallee(n);
            bestSite(n);
        }
        long loopBefore = bestLoop(n);
        long calleeBefore = bestCallee(n);
        long siteBefore = bestSite(n);
        i.redefineClasses(new ClassDefinition(Target.class, own));
        for (int w = 0; w < 3; w++) {
            bestLoop(n);
            bestCallee(n);
            bestSite(n);
        }
        long loopAfter = bestLoop(n);
        long calleeAfter = bestCallee(n);
        long siteAfter = bestSite(n);
        System.out.printf(java.util.Locale.ROOT, "loop before=%.2f after=%.2f check=%d%n",
                loopBefore / (double) n, loopAfter / (double) n, Target.loop(10));
        System.out.printf(java.util.Locale.ROOT, "callee before=%.2f after=%.2f check=%d%n",
                calleeBefore / (double) n, calleeAfter / (double) n, Caller.drive(10));
        System.out.printf(java.util.Locale.ROOT, "site before=%.2f after=%.2f check=%d%n",
                siteBefore / (double) n, siteAfter / (double) n, sink == 42 ? 1 : 0);
    }
}
