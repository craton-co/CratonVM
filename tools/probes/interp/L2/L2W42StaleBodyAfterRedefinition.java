// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 42, lane L2: a compiled body of a redefined
// method's OLD bytecode published AFTER the redefinition returned
// (docs/internal/fixed-bugs/interpreter-L2-serving-a-redefined-target-compiled-tears-a-multi-class-redefinition-FIXED-20261006.md).
//
// One class, no multi-class fence: round r installs `Q.v() == 1000 + r`, then
// publishes `done = r` (volatile). A reader thread reads `d0 = done`, then
// `Q.v()`, and counts a read older than the install that had RETURNED before
// it began (`stale`: v < 1000 + d0), and one newer than can exist yet
// (`ahead`: v > 1000 + d1 + 1, d1 read after the call). The first 8 bad
// reads are printed as `sample` lines.
//
// The path it aims at: a background compile of `Q.v` copies the method's
// bytecode under the class-manager read guard, drops it, and only then opens
// the compile's epoch witness (`compile_gate::admit`). A redefinition waiting
// for the class-manager writer runs in exactly that gap -- install, fence,
// eviction -- so the body compiled from the old bytecode was stamped newer
// than the fence and `JitCache::put` published it after the redefinition:
// every later call ran the old `Q.v` until the next redefinition. The body is
// compiled at all only when the doors tier up a redefined target
// (`invoke_fast::REDEFINED_TARGETS_TIER_UP`,
// `jit::helpers::CALLEE_TEMPLATES_SERVE_REDEFINED_TARGETS`), so with both
// switches off (dev since wave 40) this probe matches on the base as well.
//
// HotSpot 25 prints (agent; the same with -Xint):
//     stale=0 ahead=0 ran=true
//     after=1300
// and no `sample` line.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W42StaleBodyAfterRedefinition$Agent
//     Can-Redefine-Classes: true
// containing L2W42StaleBodyAfterRedefinition*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W42StaleBodyAfterRedefinition
// Without the agent both VMs print "no agent". A race probe: judged on 60
// interleaved runs against `dev`. Positive control (switches on):
// CRATONVM_DBG_JITC=1 prints
//     [cratonvm-jitc] compile of L2W42StaleBodyAfterRedefinition$Q.v()I read its bytecode before a redefinition of its class: publication refused (bytecode-read witness)
// whenever the race is taken.
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;

public class L2W42StaleBodyAfterRedefinition {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Q {
        public static int v() {
            return 1000;
        }
    }

    static final int ROUNDS = 300;
    static volatile boolean stop;
    static volatile boolean running;
    static volatile int done;
    static volatile boolean finished;
    static int stale;
    static int ahead;
    static long reads;
    static final int SAMPLES = 8;
    static final long[][] samples = new long[SAMPLES][];
    static int sampled;

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L2W42StaleBodyAfterRedefinition.class
                .getResourceAsStream("L2W42StaleBodyAfterRedefinition$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /** `bytes` with `sipush 1000; ireturn` (11 03 E8 AC) turned into `sipush 1000 + round`. */
    static byte[] version(byte[] bytes, int round) {
        byte[] b = bytes.clone();
        int value = 1000 + round;
        for (int i = 0; i + 3 < b.length; i++) {
            if (b[i] == 0x11 && b[i + 1] == 0x03 && b[i + 2] == (byte) 0xE8 && b[i + 3] == (byte) 0xAC) {
                b[i + 1] = (byte) (value >> 8);
                b[i + 2] = (byte) value;
            }
        }
        return b;
    }

    static void read() {
        int st = 0;
        int ah = 0;
        long n = 0;
        running = true;
        while (!stop) {
            int d0 = done;
            int v = Q.v();
            int d1 = done;
            boolean isStale = v < 1000 + d0;
            boolean isAhead = v > 1000 + d1 + 1;
            if (isStale) {
                st++;
            }
            if (isAhead) {
                ah++;
            }
            if ((isStale || isAhead) && sampled < SAMPLES) {
                samples[sampled++] = new long[] {v, d0, d1, n};
            }
            n++;
        }
        reads = n;
        stale = st;
        ahead = ah;
        finished = true;
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        byte[] q = bytesOf("Q");
        Q.v();
        Thread worker = new Thread(L2W42StaleBodyAfterRedefinition::read, "reader");
        worker.setDaemon(true);
        worker.start();
        while (!running) {
            Thread.onSpinWait();
        }
        for (int round = 1; round <= ROUNDS; round++) {
            i.redefineClasses(new ClassDefinition(Q.class, version(q, round)));
            done = round;
            // Let the reader call the new version often enough to nominate it
            // for a compile before the next round.
            long until = System.nanoTime() + 2_000_000L;
            while (System.nanoTime() < until) {
                Thread.onSpinWait();
            }
        }
        stop = true;
        worker.join(60_000);
        if (!finished) {
            System.out.println("reader did not finish");
            return;
        }
        System.out.println("stale=" + stale + " ahead=" + ahead + " ran=" + (reads > 0));
        System.out.println("after=" + Q.v());
        for (int k = 0; k < sampled; k++) {
            long[] s = samples[k];
            System.out.println("sample " + k + ": v=" + s[0] + " done-before=" + s[1]
                    + " done-after=" + s[2] + " read=" + s[3]);
        }
    }
}
