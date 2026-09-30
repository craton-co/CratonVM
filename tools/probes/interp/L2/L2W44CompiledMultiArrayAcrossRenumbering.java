// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 44, lane L2: compiled loops whose `multianewarray`
// sites keep running while their class is redefined over and over with a
// RENUMBERED constant pool (a class recompiled by javac after an edit,
// installed with Instrumentation.redefineClasses). JEP 109: a running
// activation keeps its own bytecode and its own constants.
// docs/internal/fixed-bugs/interpreter-L2-multianewarray-and-type-check-sites-judge-their-index-before-the-pool-read-FIXED-20261008.md
//
// On the wave-43 build a compiled `multianewarray` site judged whether its
// index needed translating from the redefinition count read WITHOUT the
// class-manager guard (`helpers.rs::multianewarray_site_index`), and
// `interpreter::multianewarray_alloc` read the pool later under a guard of its
// own: a redefinition landing in between (more likely under CPU load, and
// every such redefinition drops the site's cached plan) made it read the NEW
// pool at its OLD index -- an `InternalError` ("invalid class ref at cp#N")
// or another array class (`wrong` > 0). Wave 44 judges the index under the
// guard the pool is read under (`CompiledMultiANewArraySite`); a cached plan
// probed lock-free is kept only while no redefinition began since.
//
// `Tgt` is the class as first loaded; `Dgt` is "the edited source": the same
// members, `tag()` grown by constants of its own (among them other array
// classes) so javac numbers every constant the loop names differently. Dgt's
// class file is renamed in place to Tgt and installed over Tgt 201 times,
// alternating with Tgt's own, while WORKERS threads spin in their one
// activation of the ORIGINAL Tgt.spin (OSR-compiled early with the JIT on),
// counting every array whose class is not the one their own code names.
//
// HotSpot 25 prints (agent; the same with -Xint):
//     spin workers=4 wrong=0 threw=none
//     fresh=tagB
// On a failure CratonVM also prints `threw-message=`, the `threw-at` frames
// of the first worker that threw, and any `threw-cause=` (HotSpot none).
// A timing probe of a race: the orchestrator judges it on 60 interleaved
// runs under load, against the base (the race window is one helper call;
// a single run proves nothing either way).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W44CompiledMultiArrayAcrossRenumbering$Agent
//     Can-Redefine-Classes: true
// containing L2W44CompiledMultiArrayAcrossRenumbering*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W44CompiledMultiArrayAcrossRenumbering
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;

public class L2W44CompiledMultiArrayAcrossRenumbering {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** The element class of the arrays the loops allocate. */
    public static class Mark {
    }

    static final Class<?> MARK_1D = Mark[].class;
    static final Class<?> MARK_2D = Mark[][].class;
    static final Class<?> MARK_3D = Mark[][][].class;
    static final Class<?> INT_2D = int[][].class;

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
            int n = one;
            while (!stop) {
                Object[][] a = new Mark[n][n];
                if (a.getClass() != MARK_2D || a[0].getClass() != MARK_1D) {
                    wrong++;
                }
                Object[][][] b = new Mark[n][n][two];
                if (b.getClass() != MARK_3D) {
                    wrong++;
                }
                int[][] c = new int[two][n];
                if (c.getClass() != INT_2D || c.length != 2) {
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
            String[][] s = new String[1][two];
            long[][] l = new long[two][1];
            Object[][] o = new Object[1][1];
            if (two == pad.length() + b.length() + s.length + l.length + o.length) {
                return "twoB";
            }
            return "tagB";
        }

        public static int spin(String expect) {
            inside++;
            int wrong = 0;
            int n = one;
            while (!stop) {
                Object[][] a = new Mark[n][n];
                if (a.getClass() != MARK_2D || a[0].getClass() != MARK_1D) {
                    wrong++;
                }
                Object[][][] b = new Mark[n][n][two];
                if (b.getClass() != MARK_3D) {
                    wrong++;
                }
                int[][] c = new int[two][n];
                if (c.getClass() != INT_2D || c.length != 2) {
                    wrong++;
                }
                laps++;
            }
            return wrong;
        }
    }

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L2W44CompiledMultiArrayAcrossRenumbering.class
                .getResourceAsStream("L2W44CompiledMultiArrayAcrossRenumbering$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /** `donor`'s class file with every occurrence of its name turned into `target`'s. */
    static byte[] renamed(byte[] b, String donor, String target) {
        String from = "L2W44CompiledMultiArrayAcrossRenumbering$" + donor;
        String to = "L2W44CompiledMultiArrayAcrossRenumbering$" + target;
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
