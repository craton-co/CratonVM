// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 41, lane L2: L3W38RedefineTwoClassesSamePool with
// a reader that says WHICH way a read went wrong
// (docs/internal/fixed-bugs/interpreter-L2-serving-a-redefined-target-compiled-tears-a-multi-class-redefinition-FIXED-20261006.md).
//
// Round r installs `P.v() == 1000 + r` and `Q.v() == 1000 + r` in one
// redefineClasses call. The main thread publishes `done = r` (volatile) after
// the call returns. The reader reads `d0 = done`, then `P.v()`, then
// `Q.v()`, then `d1 = done`, and classifies every read pair:
//
//   torn     Q older than P (the L3W38 count);
//   stale-p  P older than 1000 + d0: P ran a body older than an install that
//            had RETURNED before the read began (stale compiled code; JVMTI
//            has deoptimized every dependent frame by then);
//   stale-q  the same for Q;
//   ahead    P or Q newer than 1000 + d1 + 1: a version that cannot exist yet.
//
// The first 8 bad pairs are kept and printed as `sample` lines (p, q, d0, d1,
// the reader's iteration), so a failing host run names the path: a torn pair
// with p == 1000 + d1 + 1 and q == p - 1 is a read INSIDE the window (a new P
// visible before the call returned); a torn pair with q < 1000 + d0 is stale
// compiled Q code AFTER the window (a withdrawn body still running its old
// splice).
//
// HotSpot 25 prints (agent; the same with -Xint):
//     torn=0 stale-p=0 stale-q=0 ahead=0 ran=true
//     after=1200,1200
// and no `sample` line.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W41TwoClassRedefinitionReadTrace$Agent
//     Can-Redefine-Classes: true
// containing L2W41TwoClassRedefinitionReadTrace*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W41TwoClassRedefinitionReadTrace
// Without the agent both VMs print "no agent". A race probe: the orchestrator
// judges it on 60 interleaved runs against `dev`. Positive control of the
// wave-41 compiled half of the install fence: CRATONVM_DBG_JITC=1 prints, per
// round,
//     [cratonvm-jitc] install fence: withdrew N bodies (evicted M) of 2 classes ahead of the first install; loop-exit handshake=...
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;

public class L2W41TwoClassRedefinitionReadTrace {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class P {
        public static int v() {
            return 1000;
        }
    }

    public static class Q {
        public static int v() {
            return 1000;
        }
    }

    static volatile boolean stop;
    static volatile boolean running;
    static volatile int done;
    static volatile boolean finished;
    static int torn;
    static int staleP;
    static int staleQ;
    static int ahead;
    static long reads;
    static final int SAMPLES = 8;
    static final long[][] samples = new long[SAMPLES][];
    static int sampled;

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L2W41TwoClassRedefinitionReadTrace.class
                .getResourceAsStream("L2W41TwoClassRedefinitionReadTrace$" + simple + ".class")) {
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
        int bad = 0;
        int sp = 0;
        int sq = 0;
        int ah = 0;
        long n = 0;
        running = true;
        while (!stop) {
            int d0 = done;
            int vp = P.v();
            int vq = Q.v();
            int d1 = done;
            boolean isTorn = vq < vp;
            boolean isStaleP = vp < 1000 + d0;
            boolean isStaleQ = vq < 1000 + d0;
            boolean isAhead = vp > 1000 + d1 + 1 || vq > 1000 + d1 + 1;
            if (isTorn) {
                bad++;
            }
            if (isStaleP) {
                sp++;
            }
            if (isStaleQ) {
                sq++;
            }
            if (isAhead) {
                ah++;
            }
            if ((isTorn || isStaleP || isStaleQ || isAhead) && sampled < SAMPLES) {
                samples[sampled++] = new long[] {vp, vq, d0, d1, n};
            }
            n++;
        }
        reads = n;
        torn = bad;
        staleP = sp;
        staleQ = sq;
        ahead = ah;
        finished = true;
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        byte[] p = bytesOf("P");
        byte[] q = bytesOf("Q");
        P.v();
        Q.v();
        Thread worker = new Thread(L2W41TwoClassRedefinitionReadTrace::read, "reader");
        worker.setDaemon(true);
        worker.start();
        while (!running) {
            Thread.onSpinWait();
        }
        for (int round = 1; round <= 200; round++) {
            i.redefineClasses(new ClassDefinition(P.class, version(p, round)),
                    new ClassDefinition(Q.class, version(q, round)));
            done = round;
        }
        stop = true;
        worker.join(60_000);
        if (!finished) {
            System.out.println("reader did not finish");
            return;
        }
        System.out.println("torn=" + torn + " stale-p=" + staleP + " stale-q=" + staleQ
                + " ahead=" + ahead + " ran=" + (reads > 0));
        System.out.println("after=" + P.v() + "," + Q.v());
        for (int k = 0; k < sampled; k++) {
            long[] s = samples[k];
            System.out.println("sample " + k + ": p=" + s[0] + " q=" + s[1] + " done-before=" + s[2]
                    + " done-after=" + s[3] + " read=" + s[4]);
        }
    }
}
