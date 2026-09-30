// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 37, lane L3: the line number an activation that
// was running across an IDE-HotSwap-shaped redefinition reports for itself
// afterwards. `Dgt` is "the edited source" of `Tgt` (javac output: one more
// statement and a new constant, so the pool is renumbered, and -- being a
// different stretch of this file -- another LineNumberTable), renamed in place
// to Tgt and installed with Instrumentation.redefineClasses while a worker
// spins in Tgt.lines(). JEP 109 / JVMTI: the running activation continues in
// the OBSOLETE method, whose line table is the original's; HotSpot resolves a
// stack-trace element against the method version that ran.
//
//   early   -- Tgt.lines() run to completion before any redefinition: the
//              original's line of the `new Throwable()`.
//   late    -- the same line, from the activation that was spinning when the
//              class was redefined (it leaves its loop only after).
//   fresh   -- a call made after the redefinition: the edited source's line.
//
// HotSpot 25 prints (agent; the same with -Xint):
//     late=early fresh=other
// CratonVM, expected before any fix (read from the code, not run): with the
// JIT on, the spinning loop is OSR-compiled before the redefinition and the
// compiled obsolete activation's frame is resolved from the class as it is
// now (`stackwalker::push_compiled_frames` / `reline_entry` read the class
// store's current method), so `late=other`; --nojit prints HotSpot's line
// (the interpreted frame is moved onto its own body, which carries its line
// table: `Frame::own_line_number`). See
// docs/internal/fixed-bugs/interpreter-L3-a-compiled-obsolete-activation-reports-the-new-bodys-line-numbers-FIXED-20261002.md.
// Wave 38 (lane L3): the wave-37 host run printed HotSpot's line in every
// mode because this probe never reaches a compiled obsolete activation of its
// own: `Tgt.lines` is called once before the spin, so the spinning call runs
// interpreted and at most OSRs its loop, and an OSR body runs inside its
// interpreter frame, which the redefinition's handshake moves onto its own
// body (the forced poll's `safepoint_check`); the Throwable capture then reads
// that frame's own table at the OSR bci. The method-entry shape is
// L3W38HotSwapCompiledLines's `entry` row.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W37HotSwapCompiledLines$Agent
//     Can-Redefine-Classes: true
// containing L3W37HotSwapCompiledLines*.class (compiled with line numbers,
// javac's default), then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W37HotSwapCompiledLines
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;

public class L3W37HotSwapCompiledLines {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** The class as first loaded. */
    public static class Tgt {
        static volatile boolean go;
        static volatile boolean inside;
        static volatile long spins;

        public static int lines() {
            inside = true;
            long n = 0;
            while (!go) {
                n++;
            }
            spins = n;
            Throwable t = new Throwable();
            return t.getStackTrace()[0].getLineNumber();
        }
    }

    /** The edited source, as javac compiled it: renamed to Tgt before use. */
    public static class Dgt {
        static volatile boolean go;
        static volatile boolean inside;
        static volatile long spins;

        public static int lines() {
            inside = true;
            long n = 0;
            while (!go) {
                n++;
            }
            spins = n + "edited".length();
            Throwable t = new Throwable();
            return t.getStackTrace()[0].getLineNumber();
        }
    }

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L3W37HotSwapCompiledLines.class
                .getResourceAsStream("L3W37HotSwapCompiledLines$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /** `donor`'s class file with every occurrence of its name turned into `target`'s. */
    static byte[] renamed(byte[] b, String donor, String target) {
        String from = "L3W37HotSwapCompiledLines$" + donor;
        String to = "L3W37HotSwapCompiledLines$" + target;
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

    static final int[] LATE = new int[1];

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        byte[] edited = renamed(bytesOf("Dgt"), "Dgt", "Tgt");
        Tgt.go = true;
        int early = Tgt.lines();
        Tgt.go = false;
        Tgt.inside = false;
        Thread worker = new Thread(() -> LATE[0] = Tgt.lines(), "spinner");
        worker.setDaemon(true);
        worker.start();
        while (!Tgt.inside) {
            Thread.onSpinWait();
        }
        // Long enough for a JIT to compile the spinning loop.
        Thread.sleep(500);
        i.redefineClasses(new ClassDefinition(Tgt.class, edited));
        Tgt.go = true;
        worker.join(60_000);
        Tgt.go = true;
        int fresh = Tgt.lines();
        System.out.println("late=" + (LATE[0] == early ? "early" : "other(" + LATE[0] + " vs " + early + ")")
                + " fresh=" + (fresh == early ? "early" : "other"));
    }
}
