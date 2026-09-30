// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 45, lane L2: one compiled loop of a class keeps
// running while its class is redefined ONCE with a renumbered constant pool
// (a class recompiled by javac after an edit). JEP 109: the running
// activation keeps its own bytecode and its own constants, whichever way the
// VM runs the rest of it.
// docs/known-issues/interpreter/i44-L2-proposal-an-own-class-compiled-activation-leaves-at-its-constant-pool-sites-20261008.md
//
// Before wave 45 the OSR body of `Tgt.spin` (the loop's own class) was
// spared by the redefinition's force pass and kept running compiled,
// translating every constant-pool index it reached (`ldc` String / Class,
// `new`, `anewarray`, `multianewarray`) into the merged pool. Wave 45 forces
// it and the verdict sends it to the interpreter at its next exit; the frame
// then runs the old bytecode interpreted, on its translated copy. The output
// is the same either way; what differs is the positive control:
//
//   CRATONVM_DBG_JITC=1  ->  [cratonvm-jitc] own-class OSR body leaves (renumbered pool): ...$Tgt.spin...
//   CRATONVM_DBG_DEOPT=1 ->  [cratonvm-deopt] withdrawn body told to leave: ...$Tgt.spin:(Ljava/lang/String;)I verdict=0x..
//                            [cratonvm-deopt] OSR-exit TRANSFER ...$Tgt.spin...
//
// None of the three lines names `Tgt.spin` on the base (69568bea6): its
// candidate line reads `own-class=true`, and the body is spared.
//
// HotSpot 25 prints (agent; the same with -Xint):
//     swap wrong=0 threw=none after-swap-laps=true
//     old-activation-constant=spinA
//     fresh=tagB
// On a failure CratonVM also prints `threw-message=` and the `threw-at`
// frames. A redefinition race: the orchestrator repeats it 60 times,
// interleaved against the base.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W45OwnClassLoopLeavesAtRenumbering$Agent
//     Can-Redefine-Classes: true
// containing L2W45OwnClassLoopLeavesAtRenumbering*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W45OwnClassLoopLeavesAtRenumbering
// With the single-pass OSR tier only (so the constant-pool helper sites
// exist): CRATONVM_JIT=osr-optimizing=0. Without the agent both VMs print
// "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;

public class L2W45OwnClassLoopLeavesAtRenumbering {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** The class the loop names by `ldc`, `new`, `anewarray` and `multianewarray`. */
    public static class Mark {
    }

    static final Class<?> MARK = Mark.class;
    static final Class<?> MARK_ARRAY = Mark[].class;
    static final Class<?> MARK_GRID = Mark[][].class;

    /** The class as first loaded. */
    public static class Tgt {
        static volatile boolean stop;
        static volatile long laps;
        static volatile String last;

        public static String tag() {
            return "tagA";
        }

        public static int spin(String expect) {
            int wrong = 0;
            while (!stop) {
                String s = "spinA";
                if (s != expect) {
                    wrong++;
                }
                last = s;
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
                Object[][] g = new Mark[1][1];
                if (g.getClass() != MARK_GRID) {
                    wrong++;
                }
                laps++;
            }
            return wrong;
        }
    }

    /** The edited source, as javac compiled it: renamed to Tgt before use. */
    public static class Dgt {
        static volatile boolean stop;
        static volatile long laps;
        static volatile String last;

        public static String tag() {
            String pad = "padB";
            StringBuilder b = new StringBuilder("wideB");
            if (pad.length() == b.length() + 7) {
                return "twoB";
            }
            return "tagB";
        }

        public static int spin(String expect) {
            int wrong = 0;
            while (!stop) {
                String s = "spinB";
                if (s != expect) {
                    wrong++;
                }
                last = s;
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
                Object[][] g = new Mark[1][1];
                if (g.getClass() != MARK_GRID) {
                    wrong++;
                }
                laps++;
            }
            return wrong;
        }
    }

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L2W45OwnClassLoopLeavesAtRenumbering.class
                .getResourceAsStream("L2W45OwnClassLoopLeavesAtRenumbering$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /** `donor`'s class file with every occurrence of its name turned into `target`'s. */
    static byte[] renamed(byte[] b, String donor, String target) {
        String from = "L2W45OwnClassLoopLeavesAtRenumbering$" + donor;
        String to = "L2W45OwnClassLoopLeavesAtRenumbering$" + target;
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

    static volatile int result = -1;
    static volatile Throwable thrown;

    static void waitForLaps(long atLeast) throws InterruptedException {
        long deadline = System.nanoTime() + 60_000_000_000L;
        while (Tgt.laps < atLeast && System.nanoTime() < deadline) {
            Thread.sleep(1);
        }
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        byte[] edited = renamed(bytesOf("Dgt"), "Dgt", "Tgt");
        Tgt.tag();
        Thread worker = new Thread(() -> {
            try {
                result = Tgt.spin("spinA");
            } catch (Throwable t) {
                thrown = t;
            }
        }, "spinner");
        worker.setDaemon(true);
        worker.start();
        // Well past any OSR threshold before the swap.
        waitForLaps(200_000);
        Thread.sleep(50);
        i.redefineClasses(new ClassDefinition(Tgt.class, edited));
        long atSwap = Tgt.laps;
        // The old activation must keep looping, on its own constants.
        waitForLaps(atSwap + 50_000);
        boolean moved = Tgt.laps > atSwap;
        String seen = Tgt.last;
        Tgt.stop = true;
        worker.join(60_000);
        Throwable t = thrown;
        System.out.println("swap wrong=" + result + " threw="
                + (t == null ? "none" : t.getClass().getName())
                + " after-swap-laps=" + moved
                + (worker.isAlive() ? " (still running)" : ""));
        System.out.println("old-activation-constant=" + seen);
        System.out.println("fresh=" + Tgt.tag());
        if (t != null) {
            System.out.println("threw-message=" + t.getMessage());
            for (StackTraceElement e : t.getStackTrace()) {
                System.out.println("threw-at " + e);
            }
        }
    }
}
