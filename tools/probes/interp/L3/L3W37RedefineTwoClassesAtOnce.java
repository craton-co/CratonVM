// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 37, lane L3: one Instrumentation.redefineClasses
// call on TWO classes, as an IDE's HotSwap sends every class the edit
// recompiled in one call, while another thread keeps calling both.
//
// Each class's `v()` returns a version string (`v000` as compiled). Round r
// installs version r of BOTH classes in one call (the class files with the
// literal patched to `v<r>`), 200 rounds. The worker reads `P.v()` and then
// `Q.v()` and counts the reads where Q's version is OLDER than the P version
// it read just before: that needs P to have been installed while Q was not
// yet, a state no thread may observe if the call is one operation.
//
// HotSpot 25 prints (agent; the same with -Xint):
//     torn=0 ran=true
//     after=v200,v200
// HotSpot redefines every class of the call in one VM operation at a
// safepoint (VM_RedefineClasses). CratonVM, read from the code (not run):
// `install_redefinitions` (vm/src/runtime/instrument.rs) calls
// `redefine_class_as_instrument` class by class, each its own class-manager
// write hold, so between P's swap and Q's the worker can read P's version r
// and Q's version r-1: expected `torn=` > 0 before wave 37. Since wave 37
// (lane L3) the call's classes install under one redefinition fence
// (docs/internal/fixed-bugs/interpreter-L3-a-multi-class-redefinition-is-installed-one-class-at-a-time-FIXED-20261001.md):
// expected HotSpot's lines. Positive control: CRATONVM_DBG_RETRANSFORM=1
// prints "[redefine] fence raised for a call of 2 classes" per round.
// Sensitivity, measured on HotSpot 25: the same probe with the call split in
// two (redefineClasses(P') then redefineClasses(Q')) prints torn=65232 with
// the JIT and torn=5347 with -Xint.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W37RedefineTwoClassesAtOnce$Agent
//     Can-Redefine-Classes: true
// containing L3W37RedefineTwoClassesAtOnce*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W37RedefineTwoClassesAtOnce
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;

public class L3W37RedefineTwoClassesAtOnce {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class P {
        public static String v() {
            return "v000";
        }
    }

    public static class Q {
        public static String v() {
            return "v000";
        }
    }

    static volatile boolean stop;
    static volatile boolean running;
    static volatile int torn;
    static volatile long reads;

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L3W37RedefineTwoClassesAtOnce.class
                .getResourceAsStream("L3W37RedefineTwoClassesAtOnce$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /** `bytes` with the literal `v000` turned into `v<round>` (same length). */
    static byte[] version(byte[] bytes, int round) {
        byte[] b = bytes.clone();
        String to = String.format("v%03d", round);
        for (int i = 0; i + 4 <= b.length; i++) {
            if (b[i] == 'v' && b[i + 1] == '0' && b[i + 2] == '0' && b[i + 3] == '0') {
                for (int k = 0; k < 4; k++) {
                    b[i + k] = (byte) to.charAt(k);
                }
            }
        }
        return b;
    }

    static int number(String v) {
        return Integer.parseInt(v.substring(1));
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
                String vp = P.v();
                String vq = Q.v();
                if (number(vq) < number(vp)) {
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
