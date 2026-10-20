// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 39, lane L3: the line a StackWalker frame reports
// for an activation that was running across an IDE-HotSwap-shaped
// redefinition (docs/internal/fixed-bugs/interpreter-L3-a-stack-walk-reports-a-line-for-an-obsolete-method-FIXED-20261003.md).
// Three activations of `WlkA` spin while `WlkA` is redefined with the edited
// source (`WlkB`, renamed in place: another pool, other LineNumberTables):
//   obsolete -- `lines`, whose body the edit changed, walks its own frame;
//   emcp     -- `same`, whose bytecode the edit left equal (HotSpot's EMCP;
//               its lines moved with the rest of the file), walks its own frame;
//   caller   -- `outer`, whose body the edit changed, is suspended in a call
//               to `Helper.walkCaller` (a class not redefined), which walks
//               and reads the SECOND frame: `outer`'s.
// Rows `obsolete` / `emcp` / `caller`: `late` is the frame's line, read
// inside the walk (a StackFrame resolves its line lazily in HotSpot, so it is
// read at once) by the activation that spun across the redefinition,
// compared with the same line from a call before it (`early`); `none` is a
// negative line. `fresh` is a call after the redefinition (the edited
// source's line: `other`). Row `file`: the file names of the three late
// frames, and of a fresh one.
//
// HotSpot 25 prints (agent; the same with -Xint; measured, JDK 25.0.3):
//     obsolete late=none fresh=other
//     emcp late=none fresh=other
//     caller late=none fresh=other
//     file late=null,null,null fresh=L3W39HotSwapWalkerLines.java
// A StackWalker frame of an activation begun before its class's redefinition
// -- an OLD version of its method, obsolete or EMCP; measured too with a
// redefinition by the class's own unchanged bytes -- has no line and no file
// in HotSpot, unlike a Throwable element of the same activation, which keeps
// the old body's line (L3W38HotSwapCompiledLines). The HotSpot code behind it
// was not read.
// CratonVM before wave 39, read from the code: `late=early` on every line row
// under --nojit (a moved frame, obsolete or EMCP, reports its own body's line
// table, `Frame::own_line_number`) and `late=other(..)` for a compiled
// activation (the class's current table at the old bci); the file named
// everywhere. Since
// wave 39 the walker's capture (`NativeExceptionAccess::capture_stack_walk_trace`
// -> `stackwalker::capture_stack_walk_trace`) gives such an activation no line
// and no file: the three line rows print HotSpot's lines in every mode.
// Row `file` still differed by default on wave 39: the native
// `StackWalker.walk`'s carrier read the file lazily from the class
// (docs/internal/fixed-bugs/interpreter-L3-a-stack-frames-line-and-file-are-not-read-lazily-FIXED-20261005.md,
// item 1); CRATONVM_SW_JDK_WALK=1 (the JDK's own walk) printed HotSpot's
// `file` line. Since wave 40 the carrier records an entry that names no file
// (`reflect_invoke::populate_stack_frame`, slot 7's `P59_SF_NO_FILE`) and
// answers null for it: HotSpot's `file` line on both routes.
// --compatible prints the same lines as the default.
// Positive control: CRATONVM_DBG_RETRANSFORM=1 prints
//     [redefine] stack walk: no line for an old activation: L3W39HotSwapWalkerLines$WlkA.lines ...
// (and `.same`, `.outer`) for the late walks.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W39HotSwapWalkerLines$Agent
//     Can-Redefine-Classes: true
// containing L3W39HotSwapWalkerLines*.class (compiled with line numbers,
// javac's default), then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W39HotSwapWalkerLines
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;
import java.util.function.Function;
import java.util.stream.Stream;

public class L3W39HotSwapWalkerLines {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** Flags outside the redefined class. */
    public static class Gate {
        static volatile boolean go;
        static volatile boolean inLines;
        static volatile boolean inSame;
        static volatile boolean inHelper;
        static volatile long spins;
    }

    /** A frame's line and file, read inside the walk. */
    public static final class Seen {
        final int line;
        final String file;

        Seen(StackWalker.StackFrame frame) {
            line = frame.getLineNumber();
            file = frame.getFileName();
        }
    }

    static final Function<Stream<StackWalker.StackFrame>, Seen> FIRST =
            frames -> new Seen(frames.findFirst().get());
    static final Function<Stream<StackWalker.StackFrame>, Seen> SECOND =
            frames -> new Seen(frames.skip(1).findFirst().get());

    /** Not redefined: spins, then reads its caller's line. */
    public static class Helper {
        public static Seen walkCaller(long min) {
            Gate.inHelper = true;
            long n = 0;
            while (n < min || !Gate.go) {
                n++;
            }
            Gate.spins += n;
            return StackWalker.getInstance().walk(SECOND);
        }
    }

    /** As first loaded. */
    public static class WlkA {
        public static Seen lines(long min) {
            Gate.inLines = true;
            long n = 0;
            while (n < min || !Gate.go) {
                n++;
            }
            Gate.spins += n;
            return StackWalker.getInstance().walk(FIRST);
        }

        public static Seen same(long min) {
            Gate.inSame = true;
            long n = 0;
            while (n < min || !Gate.go) {
                n++;
            }
            Gate.spins += n;
            return StackWalker.getInstance().walk(FIRST);
        }

        public static Seen outer(long min) {
            return Helper.walkCaller(min);
        }
    }

    /** The edited source: renamed to WlkA before use. */
    public static class WlkB {
        public static Seen lines(long min) {
            Gate.inLines = true;
            long n = 0;
            while (n < min || !Gate.go) {
                n++;
            }
            Gate.spins += n + "edited".length();
            return StackWalker.getInstance().walk(FIRST);
        }

        public static Seen same(long min) {
            Gate.inSame = true;
            long n = 0;
            while (n < min || !Gate.go) {
                n++;
            }
            Gate.spins += n;
            return StackWalker.getInstance().walk(FIRST);
        }

        public static Seen outer(long min) {
            Gate.spins += "edited too".length();
            return Helper.walkCaller(min);
        }
    }

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L3W39HotSwapWalkerLines.class
                .getResourceAsStream("L3W39HotSwapWalkerLines$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /** `donor`'s class file with every occurrence of its name turned into `target`'s (same length). */
    static byte[] renamed(byte[] b, String donor, String target) {
        String from = "L3W39HotSwapWalkerLines$" + donor;
        String to = "L3W39HotSwapWalkerLines$" + target;
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

    static String verdict(Seen lateFrame, Seen earlyFrame) {
        int late = lateFrame.line;
        int early = earlyFrame.line;
        return late == early ? "early" : late < 0 ? "none" : "other(" + late + " vs " + early + ")";
    }

    static String fresh(Seen freshFrame, Seen earlyFrame) {
        int fresh = freshFrame.line;
        int early = earlyFrame.line;
        return fresh == early ? "early" : fresh < 0 ? "none" : "other";
    }

    static final Seen[] LATE = new Seen[3];
    static volatile long sink;

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        byte[] edited = renamed(bytesOf("WlkB"), "WlkB", "WlkA");
        Gate.go = true;
        Seen earlyLines = WlkA.lines(20);
        Seen earlySame = WlkA.same(20);
        Seen earlyOuter = WlkA.outer(20);
        for (int round = 0; round < 2; round++) {
            for (int k = 0; k < 4_000; k++) {
                sink += WlkA.lines(20).line + WlkA.same(20).line + WlkA.outer(20).line;
            }
            Thread.sleep(300);
        }
        Gate.go = false;
        Gate.inLines = false;
        Gate.inSame = false;
        Gate.inHelper = false;
        Thread[] workers = {
            new Thread(() -> LATE[0] = WlkA.lines(0), "walker-obsolete"),
            new Thread(() -> LATE[1] = WlkA.same(0), "walker-emcp"),
            new Thread(() -> LATE[2] = WlkA.outer(0), "walker-caller"),
        };
        for (Thread w : workers) {
            w.setDaemon(true);
            w.start();
        }
        while (!Gate.inLines || !Gate.inSame || !Gate.inHelper) {
            Thread.onSpinWait();
        }
        Thread.sleep(300);
        i.redefineClasses(new ClassDefinition(WlkA.class, edited));
        Gate.go = true;
        for (Thread w : workers) {
            w.join(60_000);
        }
        System.out.println("obsolete late=" + verdict(LATE[0], earlyLines)
                + " fresh=" + fresh(WlkA.lines(0), earlyLines));
        System.out.println("emcp late=" + verdict(LATE[1], earlySame)
                + " fresh=" + fresh(WlkA.same(0), earlySame));
        System.out.println("caller late=" + verdict(LATE[2], earlyOuter)
                + " fresh=" + fresh(WlkA.outer(0), earlyOuter));
        System.out.println("file late=" + LATE[0].file + "," + LATE[1].file + "," + LATE[2].file
                + " fresh=" + WlkA.lines(0).file);
    }
}
