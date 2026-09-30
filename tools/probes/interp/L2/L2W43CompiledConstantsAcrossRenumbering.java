// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 43, lane L2: compiled loops whose constant-pool
// helper sites (ldc String, ldc Class, new, anewarray) keep running while
// their class is redefined over and over with a RENUMBERED constant pool (a
// class recompiled by javac after an edit, installed with
// Instrumentation.redefineClasses). JEP 109: a running activation keeps its
// own bytecode and its own constants.
// docs/internal/fixed-bugs/interpreter-L2-a-spinning-obsolete-frame-sometimes-throws-internalerror-across-a-renumbering-redefinition-FIXED-20261007.md
//
// On the wave-42 build a compiled site judged whether its index needed
// translating from the redefinition count read WITHOUT the class-manager
// guard, and read the pool later: a redefinition landing in between (more
// likely under CPU load) made it read the NEW pool at its OLD index -- an
// `InternalError` ("JIT ldc: cp#N ... holds Utf8(...)") or another literal /
// class (`wrong` > 0). Wave 43 judges it under the guard the pool is read
// under (`vm/src/jit/helpers.rs::CpSite`).
//
// `Tgt` is the class as first loaded; `Dgt` is "the edited source": the same
// members, `tag()` grown by constants of its own so javac numbers every
// constant the loop names differently, the loop's literal changed to "spinB".
// Dgt's class file is renamed in place to Tgt and installed over Tgt 201
// times, alternating with Tgt's own, while WORKERS threads spin in their one
// activation of the ORIGINAL Tgt.spin (OSR-compiled early with the JIT on),
// counting every constant that is not their own.
//
// HotSpot 25 prints (agent; the same with -Xint):
//     spin workers=4 wrong=0 threw=none
//     fresh=tagB
// On a failure CratonVM also prints `threw-message=`, the `threw-at` frames
// of the first worker that threw, and any `threw-cause=` (HotSpot none).
// A timing probe of a race: the orchestrator judges it on 60 interleaved
// runs under load, against the base.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W43CompiledConstantsAcrossRenumbering$Agent
//     Can-Redefine-Classes: true
// containing L2W43CompiledConstantsAcrossRenumbering*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W43CompiledConstantsAcrossRenumbering
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;

public class L2W43CompiledConstantsAcrossRenumbering {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** The class the loops name by `ldc`, `new` and `anewarray`. */
    public static class Mark {
    }

    static final Class<?> MARK = Mark.class;
    static final Class<?> MARK_ARRAY = Mark[].class;

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
                Class<?> k = Mark.class;
                if (k != MARK) {
                    wrong++;
                }
                Object o = new Mark();
                if (o.getClass() != MARK) {
                    wrong++;
                }
                Object[] a = new Mark[1];
                if (a.getClass() != MARK_ARRAY) {
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
            StringBuilder b = new StringBuilder("wideB");
            if (two == pad.length() + b.length()) {
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
                Class<?> k = Mark.class;
                if (k != MARK) {
                    wrong++;
                }
                Object o = new Mark();
                if (o.getClass() != MARK) {
                    wrong++;
                }
                Object[] a = new Mark[1];
                if (a.getClass() != MARK_ARRAY) {
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
        try (InputStream in = L2W43CompiledConstantsAcrossRenumbering.class
                .getResourceAsStream("L2W43CompiledConstantsAcrossRenumbering$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /** `donor`'s class file with every occurrence of its name turned into `target`'s. */
    static byte[] renamed(byte[] b, String donor, String target) {
        String from = "L2W43CompiledConstantsAcrossRenumbering$" + donor;
        String to = "L2W43CompiledConstantsAcrossRenumbering$" + target;
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
        byte[] original = bytesOf("Tgt");
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
        // Let the loops reach their compiled bodies before the first swap.
        Thread.sleep(200);
        for (int round = 0; round < 201; round++) {
            byte[] bytes = (round % 2 == 0) ? edited : original;
            i.redefineClasses(new ClassDefinition(Tgt.class, bytes));
            Thread.sleep(1);
        }
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
        System.out.println("spin workers=" + WORKERS + " wrong=" + wrong + " threw="
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
