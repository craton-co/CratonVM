// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 37, lane L3: an IDE-HotSwap-shaped redefinition
// (a class recompiled by javac after an edit, so its constant pool is
// RENUMBERED, installed with Instrumentation.redefineClasses) while another
// thread is running a loop of the class. JEP 109: the running activation keeps
// its own bytecode AND its own constants; only new calls run the new body.
// docs/internal/fixed-bugs/interpreter-L3-obsolete-frame-moves-are-not-atomic-with-the-redefinition-FIXED-20261001.md,
// window 1 (an instruction in flight across the swap).
//
// `Tgt` is the class as first loaded; `Dgt` is "the edited source": the same
// fields and methods, `tag()` grown by a local and a branch, the loop's literal
// changed to "spinB". javac numbers Dgt's pool differently (its `tag()` comes
// first and names new constants), so the indices Tgt.spin's getstatic / ldc /
// putstatic operands carry name OTHER entries in Dgt's pool (a Methodref, Utf8
// entries). Dgt's class file is renamed in place to Tgt (the names differ in
// one character) and installed over Tgt 101 times, alternating with Tgt's own
// class file, while a worker spins in its one activation of the ORIGINAL
// Tgt.spin, which counts every literal / static read that is not its own.
//
// HotSpot 25 prints (agent; the same with -Xint):
//     spin wrong=0 threw=none
//     fresh=tagB
// CratonVM before wave 37 (read from the code, not run): a spinning
// interpreted frame reads its caches until the redefinition advances the
// resolution epoch under the class-manager writer, and its next constant-pool
// instruction then blocks on the class-manager read lock INSIDE its slow path
// with the old index already decoded; when the writer lets it go the index is
// read in the NEW pool -- here a Methodref where a Fieldref was, a Utf8 where
// a String was -- before the thread's next poll moves the frame. Expected:
// `wrong` > 0 or a `threw=` other than none (--nojit, and with the JIT once
// the frame runs a translated copy, which is never OSR'd). With the JIT on and
// the loop OSR-compiled before the first swap, the compiled obsolete body reads
// its constants through its compile stamp and prints HotSpot's line.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W37HotSwapSpinningReads$Agent
//     Can-Redefine-Classes: true
// containing L3W37HotSwapSpinningReads*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W37HotSwapSpinningReads
// Without the agent both VMs print "no agent".
//
// On a failure the probe also prints `threw-message=`, one `threw-at` line per
// stack frame and any `threw-cause=` (wave 41, lane L3); HotSpot prints none.
// The InternalError of
// docs/internal/fixed-bugs/interpreter-L2-a-spinning-obsolete-frame-sometimes-throws-internalerror-across-a-renumbering-redefinition-FIXED-20261007.md
// was a COMPILED `spin` body's `ldc` helper judging its index before the pool
// read (wave 43, lane L2): the host run under load that named it printed
// `JIT ldc: cp#19 ... holds Utf8(...)` from `L3W41FirstSwapHoldsTheLoop`.
//
// Positive control (CratonVM, wave 37): CRATONVM_DBG_RETRANSFORM=1 prints one
//     [redefine] fence raised: L3W37HotSwapSpinningReads$Tgt moves constants ...
// line per swap (none for an append-only redefinition), and a
//     [redefine] fence lowered: ... loop-top deferrals=N
// line after it; N > 0 on some swap shows the worker's dispatch loop waited
// at its loop top instead of running an instruction across the swap.
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;

public class L3W37HotSwapSpinningReads {
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
        static volatile boolean inside;
        static long laps;

        public static String tag() {
            return "tagA";
        }

        public static int spin(String expect) {
            inside = true;
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
        static volatile boolean inside;
        static long laps;

        public static String tag() {
            String pad = "padB";
            if (two == pad.length()) {
                return "twoB";
            }
            return "tagB";
        }

        public static int spin(String expect) {
            inside = true;
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
        try (InputStream in = L3W37HotSwapSpinningReads.class
                .getResourceAsStream("L3W37HotSwapSpinningReads$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /** `donor`'s class file with every occurrence of its name turned into `target`'s. */
    static byte[] renamed(byte[] b, String donor, String target) {
        String from = "L3W37HotSwapSpinningReads$" + donor;
        String to = "L3W37HotSwapSpinningReads$" + target;
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

    static final int[] RESULT = new int[1];
    static volatile Throwable thrown;

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        byte[] original = bytesOf("Tgt");
        byte[] edited = renamed(bytesOf("Dgt"), "Dgt", "Tgt");
        Tgt.tag();
        Thread worker = new Thread(() -> {
            try {
                RESULT[0] = Tgt.spin("spinA");
            } catch (Throwable t) {
                thrown = t;
            }
        }, "spinner");
        worker.setDaemon(true);
        worker.start();
        while (!Tgt.inside) {
            Thread.onSpinWait();
        }
        for (int round = 0; round < 101; round++) {
            byte[] bytes = (round % 2 == 0) ? edited : original;
            i.redefineClasses(new ClassDefinition(Tgt.class, bytes));
            Thread.sleep(1);
        }
        Tgt.stop = true;
        worker.join(60_000);
        Throwable t = thrown;
        System.out.println("spin wrong=" + RESULT[0] + " threw="
                + (t == null ? "none" : t.getClass().getName())
                + (worker.isAlive() ? " (still running)" : ""));
        System.out.println("fresh=" + Tgt.tag());
        if (t != null) {
            // Only on a failure (HotSpot prints neither line): enough to trace
            // it -- the message names the constant-pool index and the frames
            // name the instruction (wave 41, lane L3).
            System.out.println("threw-message=" + t.getMessage());
            for (StackTraceElement e : t.getStackTrace()) {
                System.out.println("threw-at " + e);
            }
            for (Throwable c = t.getCause(); c != null; c = c.getCause()) {
                System.out.println("threw-cause=" + c);
            }
        }
    }
}
