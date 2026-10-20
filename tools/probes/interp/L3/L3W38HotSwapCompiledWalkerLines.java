// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 38, lane L3: L3W38HotSwapCompiledLines's `entry`
// row, with the line read through StackWalker instead of a Throwable. The
// spinning call runs a METHOD-ENTRY compiled body of `WlkA.lines` (warmed
// first) when `WlkA` is redefined with the edited source (`WlkB`, renamed in
// place); after the redefinition the obsolete activation leaves its loop and
// walks its own stack.
//
// HotSpot 25 prints (agent; the same with -Xint; measured):
//     walker late=none fresh=other
// (`none`: the line is negative; -1 measured). A StackWalker frame of an
// OBSOLETE method has no line in HotSpot, with or without the JIT, unlike a
// Throwable's element of the same activation (L3W38HotSwapCompiledLines: the
// old body's line). The HotSpot code behind it was not read for this probe.
// CratonVM, read from the code (not run): --nojit expected
// `walker late=early` (the interpreted frame is moved onto its own body and
// reports its own table, `Frame::own_line_number`); with the JIT on,
// expected `walker late=other(<the edited body's line at the old bci> vs <early>)`
// (`stackwalker::capture_full_trace` -> `compiled_frame_entry` reads the
// class store's CURRENT method). Both differ from HotSpot. Recorded on
// docs/internal/fixed-bugs/interpreter-L3-a-stack-walk-reports-a-line-for-an-obsolete-method-FIXED-20261003.md.
// Since wave 39 (lane L3) the walker's capture gives an old activation no
// line (`stackwalker::capture_stack_walk_trace`): HotSpot's line in every
// mode.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W38HotSwapCompiledWalkerLines$Agent
//     Can-Redefine-Classes: true
// containing L3W38HotSwapCompiledWalkerLines*.class (compiled with line
// numbers, javac's default), then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W38HotSwapCompiledWalkerLines
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;
import java.util.function.Function;
import java.util.stream.Stream;

public class L3W38HotSwapCompiledWalkerLines {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** The line of the frame that called `walk`; outside the redefined class. */
    static final Function<Stream<StackWalker.StackFrame>, Integer> FIRST_LINE =
            frames -> frames.findFirst().get().getLineNumber();

    /** As first loaded: warmed until it runs compiled. */
    public static class WlkA {
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
            return StackWalker.getInstance().walk(FIRST_LINE);
        }
    }

    /** The edited source: renamed to WlkA before use. */
    public static class WlkB {
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
            return StackWalker.getInstance().walk(FIRST_LINE);
        }
    }

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L3W38HotSwapCompiledWalkerLines.class
                .getResourceAsStream("L3W38HotSwapCompiledWalkerLines$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /** `donor`'s class file with every occurrence of its name turned into `target`'s (same length). */
    static byte[] renamed(byte[] b, String donor, String target) {
        String from = "L3W38HotSwapCompiledWalkerLines$" + donor;
        String to = "L3W38HotSwapCompiledWalkerLines$" + target;
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
    static volatile long sink;

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        byte[] edited = renamed(bytesOf("WlkB"), "WlkB", "WlkA");
        WlkA.go = true;
        int early = WlkA.lines(20);
        for (int k = 0; k < 4_000; k++) {
            sink += WlkA.lines(20);
        }
        Thread.sleep(300);
        for (int k = 0; k < 4_000; k++) {
            sink += WlkA.lines(20);
        }
        WlkA.go = false;
        WlkA.inside = false;
        Thread worker = new Thread(() -> LATE[0] = WlkA.lines(0), "walker-spinner");
        worker.setDaemon(true);
        worker.start();
        while (!WlkA.inside) {
            Thread.onSpinWait();
        }
        Thread.sleep(300);
        i.redefineClasses(new ClassDefinition(WlkA.class, edited));
        WlkA.go = true;
        worker.join(60_000);
        int fresh = WlkA.lines(0);
        int late = LATE[0];
        System.out.println("walker late=" + (late == early ? "early" : late < 0 ? "none" : "other(" + late + " vs " + early + ")")
                + " fresh=" + (fresh == early ? "early" : "other"));
    }
}
