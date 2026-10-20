// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 41, lane L3: the process's FIRST redefinition,
// renumbering the constant pool (a class recompiled by javac after an edit,
// installed with Instrumentation.redefineClasses), while other threads spin
// in loops of the class. JEP 109: the running activations keep their own
// bytecode and constants.
// docs/internal/fixed-bugs/interpreter-L2-a-spinning-obsolete-frame-sometimes-throws-internalerror-across-a-renumbering-redefinition-FIXED-20261007.md
//
// The wave-37 redefinition fence holds every other thread's dispatch loop at
// its top while a renumbering swap runs. Wave 41 read a hole in it for the
// process's first redefinition (count 0 taken for "never gave up" by
// `FrameStack::note_conversion_deferred`) and fixed it; the host then showed
// no loop top meeting the first fence at all, on the base and the fix alike
// (60/60 each, `loop-top deferrals=0`), so the probe stays as a guard of the
// first swap, not as a reproducer of the page's InternalError.
//
// `Dgt` is `Tgt` edited (tag() grows constants ahead of the loop's), renamed
// in place to Tgt; javac numbers its pool differently, so the loop's
// getstatic / ldc / putstatic operands name other entries in it.
//
// HotSpot 25 prints (agent; the same with -Xint):
//     first-swap workers=4 wrong=0 threw=none
//     fresh=tagB
// On a failure CratonVM also prints `threw-message=`, the `threw-at` frames
// and any `threw-cause=` of the first worker that threw (HotSpot none).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W41FirstSwapHoldsTheLoop$Agent
//     Can-Redefine-Classes: true
// containing L3W41FirstSwapHoldsTheLoop*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W41FirstSwapHoldsTheLoop
// Without the agent both VMs print "no agent".
//
// Diagnostics (CratonVM): CRATONVM_DBG_RETRANSFORM=1 prints
//     [redefine] fence raised: L3W41FirstSwapHoldsTheLoop$Tgt moves constants; handshake ...
//     [redefine] fence lowered: L3W41FirstSwapHoldsTheLoop$Tgt loop-top deferrals=N
// (N counts loop tops the fence held), one `[redefine] conversion deferred
// (class manager busy): ... at FILE:LINE` line per deferral naming where the
// worker was, and a `[redefine] deferred conversion done ...` line per retry
// that converted. The wave-41 host run, base and fix, --nojit: N=0 and four
// `done` lines.
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;

public class L3W41FirstSwapHoldsTheLoop {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** The class as first loaded. */
    public static class Tgt {
        static int one = 1;
        static int two = 2;
        static volatile boolean stop;
        static volatile int inside;
        static long laps;

        public static String tag() {
            return "tagA";
        }

        public static int spin(String expect) {
            inside++;
            int wrong = 0;
            while (!stop) {
                String s = "spinA";
                if (s != expect) {
                    wrong++;
                }
                if (one != 1) {
                    wrong++;
                }
                laps++;
            }
            return wrong;
        }
    }

    /** The edited source, as javac compiled it: renamed to Tgt before use. */
    public static class Dgt {
        static int one = 1;
        static int two = 2;
        static volatile boolean stop;
        static volatile int inside;
        static long laps;

        public static String tag() {
            String pad = "padB";
            if (two == pad.length()) {
                return "twoB";
            }
            return "tagB";
        }

        public static int spin(String expect) {
            inside++;
            int wrong = 0;
            while (!stop) {
                String s = "spinB";
                if (s != expect) {
                    wrong++;
                }
                if (one != 1) {
                    wrong++;
                }
                laps++;
            }
            return wrong;
        }
    }

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L3W41FirstSwapHoldsTheLoop.class
                .getResourceAsStream("L3W41FirstSwapHoldsTheLoop$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /** `donor`'s class file with every occurrence of its name turned into `target`'s. */
    static byte[] renamed(byte[] b, String donor, String target) {
        String from = "L3W41FirstSwapHoldsTheLoop$" + donor;
        String to = "L3W41FirstSwapHoldsTheLoop$" + target;
        for (int i = 0; i + from.length() <= b.length; i++) {
            boolean match = true;
            for (int k = 0; k < from.length() && match; k++) {
                match = b[i + k] == (byte) from.charAt(k);
            }
            if (match) {
                for (int k = 0; k < to.length(); k++) {
                    b[i + k] = (byte) to.charAt(k);
                }
            }
        }
        return b;
    }

    static final int WORKERS = 4;
    static final int[] RESULT = new int[WORKERS];
    static final Throwable[] THROWN = new Throwable[WORKERS];

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        // Read and rename before any worker starts: the swap follows the last
        // worker's first lap at once.
        byte[] edited = renamed(bytesOf("Dgt"), "Dgt", "Tgt");
        Tgt.tag();
        Thread[] workers = new Thread[WORKERS];
        for (int w = 0; w < WORKERS; w++) {
            final int me = w;
            workers[w] = new Thread(() -> {
                try {
                    RESULT[me] = Tgt.spin("spinA");
                } catch (Throwable t) {
                    THROWN[me] = t;
                }
            }, "spinner-" + w);
            workers[w].setDaemon(true);
            workers[w].start();
            while (Tgt.inside <= w) {
                Thread.onSpinWait();
            }
        }
        i.redefineClasses(new ClassDefinition(Tgt.class, edited));
        Thread.sleep(20);
        Tgt.stop = true;
        int wrong = 0;
        boolean alive = false;
        Throwable first = null;
        for (int w = 0; w < WORKERS; w++) {
            workers[w].join(60_000);
            alive |= workers[w].isAlive();
            wrong += RESULT[w];
            if (first == null) {
                first = THROWN[w];
            }
        }
        System.out.println("first-swap workers=" + WORKERS + " wrong=" + wrong + " threw="
                + (first == null ? "none" : first.getClass().getName())
                + (alive ? " (still running)" : ""));
        System.out.println("fresh=" + Tgt.tag());
        if (first != null) {
            System.out.println("threw-message=" + first.getMessage());
            for (StackTraceElement e : first.getStackTrace()) {
                System.out.println("threw-at " + e);
            }
            for (Throwable c = first.getCause(); c != null; c = c.getCause()) {
                System.out.println("threw-cause=" + c);
            }
        }
    }
}
