// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;

/**
 * JIT j27 wave 5, lane X2LCAP: a caller that baked a direct call
 * (`invokestatic`) into a compiled callee keeps calling the right code when
 * that callee's body is replaced while the caller runs. Pages
 * docs/known-issues/jit/t1-X2-ir-direct-binds-plant-no-retire-cell-20261026.md
 * and t1-lcap-context-abi-floor-misses-the-driver-context-reasons-20261026.md.
 *
 * Rows:
 *  - deopt: `pick` is warmed with positive inputs only (its negative arm
 *    never runs), then a caller loop switches to negative inputs halfway, so
 *    the direct-bound callee's speculation fails mid-loop and its body is
 *    evicted while the caller still runs. Repeated after the recompile.
 *  - supersede: a cold caller loop calls a cold callee 2 048 000 times; the
 *    callee tiers up (single-pass, then optimizing) while the caller's OSR
 *    body has already baked its first body. Repeated three times.
 *  - redefine (needs the agent, see SETUP): thread `caller` loops on
 *    `Target.value()`; main redefines `Target` (constant 1000003 -> 2000003)
 *    while it runs, then raises `phase`. A call that starts after the caller
 *    read `phase == 1` began after `redefineClasses` returned, so it must run
 *    the new body (JVMS 5.4.6 / JVMTI RedefineClasses: new invocations use
 *    the new method). The caller is not redefined, so its own activation is
 *    not obsolete.
 *
 * SETUP: a jar whose manifest has
 *     Premain-Class: J27DirectBindRetire$Agent
 *     Can-Redefine-Classes: true
 * containing J27DirectBindRetire*.class, then
 *     java|cratonvm [--compatible] -javaagent:probe.jar -cp probe.jar J27DirectBindRetire
 * Without the agent both VMs print `fact redefine no-agent` in place of the
 * three redefine lines.
 *
 * Expected HotSpot 25 output (with the agent; all lines, in order):
 *
 *   fact deopt-warm 600000
 *   fact deopt-mid-loop 2200000
 *   fact deopt-after-recompile 2200000
 *   fact deopt-negative-only 1600000
 *   fact supersede-mid-run 1061888000
 *   fact supersede-repeat-mismatches 0
 *   fact redefine-after-calls-positive true
 *   fact redefine-after-old-values 0
 *   fact redefine-direct-after 2000003
 *
 * `-Dj27.time=1` adds `time <row> <ms>` lines (not compared).
 *
 * Arms (every arm prints the HotSpot lines, both modes):
 *   default
 *   CRATONVM_C2_ACCEPT=always
 *   CRATONVM_JIT_INVALIDATE_NOT_ENTRANT=1
 *   CRATONVM_JIT_INVALIDATE_NOT_ENTRANT=1 CRATONVM_JIT_INVALIDATION_RETIRES_SUPERSEDED=1
 *   (the same) CRATONVM_JIT_INVALIDATE_NOT_ENTRANT_SUPERSEDED=0
 *   CRATONVM_JIT_TRAP_EVICT_NOT_ENTRANT=0 CRATONVM_JIT_RETIRE_CELL=0   (old behaviour: answers still SAME, deopt row slower)
 *   --nojit                                                           (reference)
 */
public final class J27DirectBindRetire {
    static volatile Instrumentation inst;
    static final boolean TIME = Boolean.getBoolean("j27.time");

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    static void time(String row, long startNs) {
        if (TIME) {
            System.out.println("time " + row + " " + (System.nanoTime() - startNs) / 1_000_000);
        }
    }

    // ---- deopt of a direct-bound callee ----

    static int pick(int[] a, int i) {
        int v = a[i & (a.length - 1)];
        if (v < 0) {
            return -v * 3 + 1;
        }
        return v + 1;
    }

    static long driveOne(int[] a, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += pick(a, i);
        }
        return s;
    }

    static long driveSwitch(int[] pos, int[] neg, int n) {
        long s = 0;
        int half = n / 2;
        for (int i = 0; i < n; i++) {
            s += pick(i < half ? pos : neg, i);
        }
        return s;
    }

    static void deoptRows() {
        int[] pos = filled(5);
        int[] neg = filled(-5);
        long warm = 0;
        for (int r = 0; r < 20; r++) {
            warm = driveOne(pos, 100_000);
        }
        fact("deopt-warm", warm);
        long t = System.nanoTime();
        fact("deopt-mid-loop", driveSwitch(pos, neg, 200_000));
        time("deopt-mid-loop", t);
        long again = 0;
        for (int r = 0; r < 10; r++) {
            again = driveSwitch(pos, neg, 200_000);
        }
        fact("deopt-after-recompile", again);
        fact("deopt-negative-only", driveOne(neg, 100_000));
    }

    static int[] filled(int v) {
        int[] a = new int[1024];
        java.util.Arrays.fill(a, v);
        return a;
    }

    // ---- supersede while the caller runs ----

    static int inc(int x) {
        return x + 7;
    }

    static long driveInc(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += inc(i & 1023);
        }
        return s;
    }

    static void supersedeRows() {
        final int n = 1024 * 2000;
        long t = System.nanoTime();
        long first = driveInc(n);
        time("supersede-mid-run", t);
        fact("supersede-mid-run", first);
        int mismatches = 0;
        for (int r = 0; r < 3; r++) {
            if (driveInc(n) != first) {
                mismatches++;
            }
        }
        fact("supersede-repeat-mismatches", mismatches);
    }

    // ---- redefinition while the caller runs ----

    public static class Target {
        public static int value() {
            return 1000003;
        }
    }

    static volatile int phase;
    static volatile boolean stop;

    /// out[0] = calls after phase 1, out[1] = those that returned the old value.
    static void spinCalls(long[] out) {
        long after = 0;
        long oldAfter = 0;
        while (!stop) {
            int p = phase;
            int v = Target.value();
            if (p == 1) {
                after++;
                if (v != 2000003) {
                    oldAfter++;
                }
            }
        }
        out[0] = after;
        out[1] = oldAfter;
    }

    static byte[] targetBytes() throws Exception {
        try (InputStream in = J27DirectBindRetire.class
                .getResourceAsStream("J27DirectBindRetire$Target.class")) {
            return in.readAllBytes();
        }
    }

    /// `bytes` with its one CONSTANT_Integer 1000003 turned into 2000003.
    static byte[] patched(byte[] bytes) {
        byte[] b = bytes.clone();
        int hits = 0;
        for (int i = 0; i + 5 <= b.length; i++) {
            if (b[i] == 3 && b[i + 1] == 0x00 && b[i + 2] == 0x0F && b[i + 3] == 0x42
                    && b[i + 4] == 0x43) {
                b[i + 2] = 0x1E;
                b[i + 3] = (byte) 0x84;
                b[i + 4] = (byte) 0x83;
                hits++;
            }
        }
        if (hits != 1) {
            throw new AssertionError("constant found " + hits + " times");
        }
        return b;
    }

    static void redefineRows() throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported()) {
            fact("redefine", "no-agent");
            return;
        }
        byte[] redefined = patched(targetBytes());
        long[] out = new long[2];
        Thread caller = new Thread(() -> spinCalls(out), "caller");
        caller.start();
        Thread.sleep(400);
        i.redefineClasses(new ClassDefinition(Target.class, redefined));
        phase = 1;
        Thread.sleep(400);
        stop = true;
        caller.join(60_000);
        fact("redefine-after-calls-positive", out[0] > 0);
        fact("redefine-after-old-values", out[1]);
        int k = 0;
        for (int c = 0; c < 50_000; c++) {
            k = Target.value();
        }
        fact("redefine-direct-after", k);
    }

    public static void main(String[] args) throws Exception {
        deoptRows();
        supersedeRows();
        redefineRows();
    }
}
