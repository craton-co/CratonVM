// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 40, lane L3: the line ANOTHER thread's
// `Thread.getStackTrace()` reports for an activation that runs across an
// IDE-HotSwap-shaped redefinition of its class
// (docs/internal/fixed-bugs/interpreter-L3-another-threads-trace-gives-a-compiled-obsolete-activation-the-new-bodys-line-FIXED-20261004.md).
// `WA.spin` is warmed until it runs compiled, then a worker spins in it while
// `WA` is redefined with the edited source (`WB`, renamed in place: another
// pool, another LineNumberTable, its lines far below the old ones). The main
// thread reads `worker.getStackTrace()` before the redefinition (`early`) and
// after it while the worker still spins in the same activation (`late`).
//   late -- `old` when the late line lies within 3 lines of the early one (the
//           old body's loop), `edited` when it lies in `WB`'s lines, else
//           `other(<line>)`.
//   fresh -- a second worker that enters `WA.spin` after the redefinition:
//           `edited` (the edited source's lines).
//
// HotSpot 25 prints (agent; the same with -Xint; measured, JDK 25.0.3):
//     other-thread late=old fresh=edited
// CratonVM before wave 40, read from the code: with the JIT, `late=edited`
// or `other(..)` when the spinning call runs its method-entry compiled body:
// the published entry of a compiled activation carried no line and the
// reader resolved it from the class as it is then (the edited body's table
// at the old body's bci). --nojit printed HotSpot's line (interpreted frames
// are moved onto their own body and report its lines). Since wave 40 the
// publishing thread resolves such an entry at its compile stamp
// (`stackwalker::publish_compiled_lines_at_their_stamps`).
// --compatible prints the same line as the default.
// Positive control (JIT on): CRATONVM_DBG_RETRANSFORM=1 prints
//     [redefine] compiled frame resolved at its compile stamp: L3W40OtherThreadObsoleteLine$WA.spin ...
// for the late read; CRATONVM_DBG_STTRACE=1 prints `[sttrace] publish:` lines
// with compiled-activations=1 or more for the worker.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W40OtherThreadObsoleteLine$Agent
//     Can-Redefine-Classes: true
// containing L3W40OtherThreadObsoleteLine*.class (compiled with line numbers,
// javac's default), then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W40OtherThreadObsoleteLine
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;

public class L3W40OtherThreadObsoleteLine {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** Flags outside the redefined class. */
    public static class Gate {
        static volatile boolean go;
        static volatile boolean inside;
        static volatile long spins;
    }

    /** As first loaded. */
    public static class WA {
        public static long spin(long min) {
            Gate.inside = true;
            long n = 0;
            while (n < min || !Gate.go) {
                n++;
            }
            Gate.spins += n;
            return n;
        }
    }

    static volatile long sink;
    static final int PAD_0 = 0;
    static final int PAD_1 = 1;
    static final int PAD_2 = 2;
    static final int PAD_3 = 3;
    static final int PAD_4 = 4;
    static final int PAD_5 = 5;
    static final int PAD_6 = 6;
    static final int PAD_7 = 7;
    static final int PAD_8 = 8;
    static final int PAD_9 = 9;

    /** The edited source: renamed to WA before use. Its lines lie below every old one. */
    public static class WB {
        public static long spin(long min) {
            Gate.inside = true;
            long n = 0;
            Gate.spins += "edited".length();
            while (n < min || !Gate.go) {
                n++;
            }
            Gate.spins += n;
            return n;
        }
    }

    static final int WB_FIRST_LINE = 89;
    static final int WB_LAST_LINE = 98;

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L3W40OtherThreadObsoleteLine.class
                .getResourceAsStream("L3W40OtherThreadObsoleteLine$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /** `donor`'s class file with every occurrence of its name turned into `target`'s (same length). */
    static byte[] renamed(byte[] b, String donor, String target) {
        String from = "L3W40OtherThreadObsoleteLine$" + donor;
        String to = "L3W40OtherThreadObsoleteLine$" + target;
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

    /** The line of `WA.spin` in `t`'s trace, or MIN_VALUE when absent. */
    static int spinLine(Thread t) {
        for (StackTraceElement e : t.getStackTrace()) {
            if (e.getClassName().endsWith("$WA") && e.getMethodName().equals("spin")) {
                return e.getLineNumber();
            }
        }
        return Integer.MIN_VALUE;
    }

    static String verdict(int line, int early) {
        if (line == Integer.MIN_VALUE) {
            return "absent";
        }
        if (Math.abs(line - early) <= 3) {
            return "old";
        }
        if (line >= WB_FIRST_LINE && line <= WB_LAST_LINE) {
            return "edited";
        }
        return "other(" + line + ")";
    }

    /** The line of `WA.spin` in `t`'s trace once it spins there. */
    static int lineWhileSpinning(Thread t) throws Exception {
        int line = Integer.MIN_VALUE;
        for (int tries = 0; tries < 200 && line == Integer.MIN_VALUE; tries++) {
            line = spinLine(t);
            if (line == Integer.MIN_VALUE) {
                Thread.sleep(10);
            }
        }
        return line;
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        byte[] edited = renamed(bytesOf("WB"), "WB", "WA");
        // Warm the method whole (both loop branches taken), so the spinning
        // call enters its method-entry compiled body.
        Gate.go = true;
        for (int round = 0; round < 2; round++) {
            for (int k = 0; k < 4_000; k++) {
                sink += WA.spin(20);
            }
            Thread.sleep(300);
        }
        Gate.go = false;
        Gate.inside = false;
        Thread worker = new Thread(() -> sink += WA.spin(0), "obsolete-spinner");
        worker.setDaemon(true);
        worker.start();
        while (!Gate.inside) {
            Thread.onSpinWait();
        }
        Thread.sleep(300);
        int early = lineWhileSpinning(worker);
        i.redefineClasses(new ClassDefinition(WA.class, edited));
        Thread.sleep(100);
        int late = lineWhileSpinning(worker);
        Gate.inside = false;
        Thread fresh = new Thread(() -> sink += WA.spin(0), "fresh-spinner");
        fresh.setDaemon(true);
        fresh.start();
        while (!Gate.inside) {
            Thread.onSpinWait();
        }
        Thread.sleep(100);
        int freshLine = lineWhileSpinning(fresh);
        Gate.go = true;
        worker.join(60_000);
        fresh.join(60_000);
        System.out.println("other-thread late=" + verdict(late, early)
                + " fresh=" + verdict(freshLine, early));
    }
}
