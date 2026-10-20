// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 38, lane L3: the line an activation that was
// running across an IDE-HotSwap-shaped redefinition reports for itself
// afterwards, in the two shapes a running loop can be compiled in. The
// wave-37 probe (L3W37HotSwapCompiledLines) matched HotSpot on the host in
// every mode although its page expected a difference: its target was called
// once before the spin, so the spinning call ran INTERPRETED and at most
// OSR'd its loop, and an OSR body runs inside its interpreter frame, which
// the redefinition's handshake moves onto its own body (the compiled body's
// poll runs `safepoint_check`, which converts the thread's frames), and a
// moved frame reports its own line table (`Frame::own_line_number`, read by
// the Throwable capture at the OSR bci). Row `osr` keeps that shape; row
// `entry` warms its target first, so the spinning call enters a METHOD-ENTRY
// compiled body: a compiled activation with no interpreter frame of its own,
// which is the obsolete activation the i37 page is about.
//
// Each row: `<X>B` is "the edited source" of `<X>A` (javac output: one more
// statement and a new constant, so the pool is renumbered, and another
// LineNumberTable), renamed in place to `<X>A` and installed with
// Instrumentation.redefineClasses while a worker spins in `<X>A.lines(0)`.
//   late  -- the line of the `new Throwable()`, from the activation that was
//            spinning at the redefinition (it leaves its loop only after),
//            compared with the same line from a call made before it (early).
//   fresh -- a call after the redefinition: the edited source's line.
//
// HotSpot 25 prints (agent; the same with -Xint):
//     osr late=early fresh=other
//     entry late=early fresh=other
// CratonVM before wave 38, read from the code: `osr` as HotSpot (see above);
// `entry` with the JIT on printed `late=other(..)`: the compiled frame is
// retained as `BacktraceFrame::Compiled` with no line and resolved when read
// against the class as it is then -- the edited body's table at the old
// body's bci. Since wave 38 the retained compiled frame carries its
// compilation's constant-pool stamp and is resolved against the table its
// class had at that stamp (`obsolete_frames::resolve_lines_as_captured`).
// --nojit prints HotSpot's lines either way (interpreted frames are moved).
// Positive control (JIT on): CRATONVM_DBG_RETRANSFORM=1 prints
//     [redefine] compiled frame resolved at its compile stamp: L3W38HotSwapCompiledLines$EntA.lines ...
// for the `entry` row; CRATONVM_DBG_JITC=1 shows the method-entry compile of
// `L3W38HotSwapCompiledLines$EntA.lines(I)I` before the redefinition.
// See docs/internal/fixed-bugs/interpreter-L3-a-compiled-obsolete-activation-reports-the-new-bodys-line-numbers-FIXED-20261002.md.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W38HotSwapCompiledLines$Agent
//     Can-Redefine-Classes: true
// containing L3W38HotSwapCompiledLines*.class (compiled with line numbers,
// javac's default), then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W38HotSwapCompiledLines
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;

public class L3W38HotSwapCompiledLines {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** Row `osr`, as first loaded: called once before the spin. */
    public static class OsrA {
        static volatile boolean go;
        static volatile boolean inside;
        static volatile long spins;

        public static int lines(int min) {
            inside = true;
            long n = 0;
            while (n < min || !go) {
                n++;
            }
            spins = n;
            Throwable t = new Throwable();
            return t.getStackTrace()[0].getLineNumber();
        }
    }

    /** Row `osr`, the edited source: renamed to OsrA before use. */
    public static class OsrB {
        static volatile boolean go;
        static volatile boolean inside;
        static volatile long spins;

        public static int lines(int min) {
            inside = true;
            long n = 0;
            while (n < min || !go) {
                n++;
            }
            spins = n + "edited".length();
            Throwable t = new Throwable();
            return t.getStackTrace()[0].getLineNumber();
        }
    }

    /** Row `entry`, as first loaded: warmed until it runs compiled. */
    public static class EntA {
        static volatile boolean go;
        static volatile boolean inside;
        static volatile long spins;

        public static int lines(int min) {
            inside = true;
            long n = 0;
            while (n < min || !go) {
                n++;
            }
            spins = n;
            Throwable t = new Throwable();
            return t.getStackTrace()[0].getLineNumber();
        }
    }

    /** Row `entry`, the edited source: renamed to EntA before use. */
    public static class EntB {
        static volatile boolean go;
        static volatile boolean inside;
        static volatile long spins;

        public static int lines(int min) {
            inside = true;
            long n = 0;
            while (n < min || !go) {
                n++;
            }
            spins = n + "edited".length();
            Throwable t = new Throwable();
            return t.getStackTrace()[0].getLineNumber();
        }
    }

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L3W38HotSwapCompiledLines.class
                .getResourceAsStream("L3W38HotSwapCompiledLines$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /** `donor`'s class file with every occurrence of its name turned into `target`'s (same length). */
    static byte[] renamed(byte[] b, String donor, String target) {
        String from = "L3W38HotSwapCompiledLines$" + donor;
        String to = "L3W38HotSwapCompiledLines$" + target;
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

    static final int[] LATE = new int[2];
    static volatile long sink;

    static String row(String name, int early, int late, int fresh) {
        return name + " late=" + (late == early ? "early" : "other(" + late + " vs " + early + ")")
                + " fresh=" + (fresh == early ? "early" : "other");
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported()) {
            System.out.println("no agent");
            return;
        }

        // Row osr: one call, then the spinning call runs interpreted and may
        // OSR its loop.
        byte[] osrEdited = renamed(bytesOf("OsrB"), "OsrB", "OsrA");
        OsrA.go = true;
        int osrEarly = OsrA.lines(0);
        OsrA.go = false;
        Thread osrWorker = new Thread(() -> LATE[0] = OsrA.lines(0), "osr-spinner");
        osrWorker.setDaemon(true);
        osrWorker.start();
        while (!OsrA.inside) {
            Thread.onSpinWait();
        }
        Thread.sleep(500);
        i.redefineClasses(new ClassDefinition(OsrA.class, osrEdited));
        OsrA.go = true;
        osrWorker.join(60_000);
        int osrFresh = OsrA.lines(0);
        System.out.println(row("osr", osrEarly, LATE[0], osrFresh));

        // Row entry: warm the method whole (both loop branches taken), so the
        // spinning call enters its method-entry compiled body.
        byte[] entEdited = renamed(bytesOf("EntB"), "EntB", "EntA");
        EntA.go = true;
        int entEarly = EntA.lines(20);
        for (int k = 0; k < 4_000; k++) {
            sink += EntA.lines(20);
        }
        Thread.sleep(300);
        for (int k = 0; k < 4_000; k++) {
            sink += EntA.lines(20);
        }
        EntA.go = false;
        EntA.inside = false;
        Thread entWorker = new Thread(() -> LATE[1] = EntA.lines(0), "entry-spinner");
        entWorker.setDaemon(true);
        entWorker.start();
        while (!EntA.inside) {
            Thread.onSpinWait();
        }
        Thread.sleep(300);
        i.redefineClasses(new ClassDefinition(EntA.class, entEdited));
        EntA.go = true;
        entWorker.join(60_000);
        int entFresh = EntA.lines(0);
        System.out.println(row("entry", entEarly, LATE[1], entFresh));
    }
}
