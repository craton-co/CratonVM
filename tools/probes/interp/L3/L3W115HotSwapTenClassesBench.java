// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 115, lane L3 (proposal
// `i38-L3-proposal-drop-the-post-swap-conversion-handshake`;
// `CRATONVM_REDEFINE_NO_POST_SWAP_HANDSHAKE`, default off): one
// Instrumentation.redefineClasses call on TEN classes, as an IDE HotSwap of
// an edit that recompiled ten classes, while two threads keep calling them.
// Each round patches the literal `v000` of every class to `v<round>` (an old
// constant-pool index then names another constant, so the redefinition
// "moves constants" and today takes one post-swap handshake per class).
//
// Correctness lines (compared): the readers never see C9 older than C0 within
// one sweep (the call is one operation), and every class answers the last
// version. Timing lines (`TIME`, not compared): the mean wall time of one
// call. Expected with the switch on: the same compared lines, and the TIME
// rows lower by about nine handshakes per call (one pause per call at most
// instead of one per class). CRATONVM_DBG_RETRANSFORM=1 prints
// "[redefine] post-swap handshake skipped" per class with the switch on.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W115HotSwapTenClassesBench$Agent
//     Can-Redefine-Classes: true
// containing L3W115HotSwapTenClassesBench*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W115HotSwapTenClassesBench
//     CRATONVM_REDEFINE_NO_POST_SWAP_HANDSHAKE=1 cratonvm ... (the switch arm)
// Without the agent both VMs print "no agent".
//
// HotSpot 25 prints, with the agent (no HOTSPOT_EXPECTED block: the battery runs no agent):
// torn=0 ran=true
// after=v100,v100,v100,v100,v100,v100,v100,v100,v100,v100
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;

public class L3W115HotSwapTenClassesBench {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class C0 { public static String v() { return "v000"; } }
    public static class C1 { public static String v() { return "v000"; } }
    public static class C2 { public static String v() { return "v000"; } }
    public static class C3 { public static String v() { return "v000"; } }
    public static class C4 { public static String v() { return "v000"; } }
    public static class C5 { public static String v() { return "v000"; } }
    public static class C6 { public static String v() { return "v000"; } }
    public static class C7 { public static String v() { return "v000"; } }
    public static class C8 { public static String v() { return "v000"; } }
    public static class C9 { public static String v() { return "v000"; } }

    static final Class<?>[] CLASSES = {
        C0.class, C1.class, C2.class, C3.class, C4.class,
        C5.class, C6.class, C7.class, C8.class, C9.class,
    };

    static volatile boolean stop;
    static volatile int running;
    static final int[] torn = new int[2];
    static final long[] reads = new long[2];

    static byte[] bytesOf(Class<?> c) throws Exception {
        String simple = c.getName().substring(c.getName().lastIndexOf('.') + 1);
        try (InputStream in = L3W115HotSwapTenClassesBench.class.getResourceAsStream(simple + ".class")) {
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

    static void sweepFirstLast(int[] out) {
        out[0] = number(C0.v());
        number(C1.v());
        number(C2.v());
        number(C3.v());
        number(C4.v());
        number(C5.v());
        number(C6.v());
        number(C7.v());
        number(C8.v());
        out[1] = number(C9.v());
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        byte[][] original = new byte[CLASSES.length][];
        for (int c = 0; c < CLASSES.length; c++) {
            original[c] = bytesOf(CLASSES[c]);
        }
        int[] warm = new int[2];
        sweepFirstLast(warm);
        Thread[] workers = new Thread[2];
        for (int w = 0; w < workers.length; w++) {
            final int slot = w;
            workers[w] = new Thread(() -> {
                int bad = 0;
                long n = 0;
                int[] seen = new int[2];
                synchronized (L3W115HotSwapTenClassesBench.class) {
                    running++;
                }
                while (!stop) {
                    sweepFirstLast(seen);
                    if (seen[1] < seen[0]) {
                        bad++;
                    }
                    n++;
                }
                reads[slot] = n;
                torn[slot] = bad;
            }, "reader-" + w);
            workers[w].setDaemon(true);
            workers[w].start();
        }
        while (running < workers.length) {
            Thread.onSpinWait();
        }
        int rounds = 100;
        long start = System.nanoTime();
        for (int round = 1; round <= rounds; round++) {
            ClassDefinition[] call = new ClassDefinition[CLASSES.length];
            for (int c = 0; c < CLASSES.length; c++) {
                call[c] = new ClassDefinition(CLASSES[c], version(original[c], round));
            }
            i.redefineClasses(call);
        }
        long elapsed = System.nanoTime() - start;
        stop = true;
        for (Thread w : workers) {
            w.join(60_000);
        }
        System.out.println("torn=" + (torn[0] + torn[1]) + " ran=" + (reads[0] > 0 && reads[1] > 0));
        StringBuilder after = new StringBuilder("after=");
        for (int c = 0; c < CLASSES.length; c++) {
            if (c > 0) {
                after.append(',');
            }
            try {
                after.append(CLASSES[c].getMethod("v").invoke(null));
            } catch (ReflectiveOperationException e) {
                after.append(e.getClass().getSimpleName());
            }
        }
        System.out.println(after);
        System.out.println("TIME redefine-10-classes-us " + (elapsed / rounds / 1000));
    }
}
