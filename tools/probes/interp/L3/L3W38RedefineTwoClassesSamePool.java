// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 38, lane L3: L3W37RedefineTwoClassesAtOnce with
// class files whose constant pools do not change -- the shape of a
// Mockito / Byte Buddy retransform of a type hierarchy, where every old index
// keeps its constant. Each class's `v()` is `sipush 1000; ireturn`; round r
// installs BOTH classes with the operand patched to 1000 + r in one
// redefineClasses call, 200 rounds, while a worker reads `P.v()` then
// `Q.v()` and counts the reads where Q's version is OLDER than P's.
//
// HotSpot 25 prints (agent; the same with -Xint):
//     torn=0 ran=true
//     after=1200,1200
// CratonVM, read from the code (not run): since wave 38 a call whose classes
// move no constant raises the call-wide fence WITHOUT its handshake
// (`obsolete_frames::raise_install_fence`); the fence alone holds every other
// interpreter loop at its top from the first install to the last, so the
// expected output is HotSpot's. Positive control: CRATONVM_DBG_RETRANSFORM=1
// prints, once per round,
//     [redefine] fence raised for a call of 2 classes; handshake skipped (no class moves a constant)
// (L3W37RedefineTwoClassesAtOnce, whose patched string literal does move a
// constant, prints `handshake every peer polled` / `froze peers` instead.)
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W38RedefineTwoClassesSamePool$Agent
//     Can-Redefine-Classes: true
// containing L3W38RedefineTwoClassesSamePool*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W38RedefineTwoClassesSamePool
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;

public class L3W38RedefineTwoClassesSamePool {
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
    static volatile int torn;
    static volatile long reads;

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L3W38RedefineTwoClassesSamePool.class
                .getResourceAsStream("L3W38RedefineTwoClassesSamePool$" + simple + ".class")) {
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
        Thread worker = new Thread(() -> {
            int bad = 0;
            long n = 0;
            running = true;
            while (!stop) {
                int vp = P.v();
                int vq = Q.v();
                if (vq < vp) {
                    bad++;
                }
                n++;
            }
            reads = n;
            torn = bad;
        }, "reader");
        worker.setDaemon(true);
        worker.start();
        while (!running) {
            Thread.onSpinWait();
        }
        for (int round = 1; round <= 200; round++) {
            i.redefineClasses(new ClassDefinition(P.class, version(p, round)),
                    new ClassDefinition(Q.class, version(q, round)));
        }
        stop = true;
        worker.join(60_000);
        System.out.println("torn=" + torn + " ran=" + (reads > 0));
        System.out.println("after=" + P.v() + "," + Q.v());
    }
}
