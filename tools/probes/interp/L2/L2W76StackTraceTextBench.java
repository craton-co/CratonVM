// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 76, lane L2 -- what a stack trace's TEXT costs
// (StackTraceElement.toString(), printStackTrace) and what the element fill
// costs per frame, on a trace through 12 distinct classes. Bench for
// docs/internal/fixed-bugs/interpreter-i75-L2-proposal-jdk-only-nojit-print-stack-trace-runs-the-text-path-in-java-20261108-DONE-20261109.md
// and docs/internal/fixed-bugs/interpreter-i75-L2-proposal-element-fill-reads-the-captured-trace-in-place-20261108-DONE-20261109.md.
//
// Switches (opt-in, default OFF; the same elements and text either way):
//   CRATONVM_STACK_TRACE_ELEMENT_TO_STRING_INTRINSIC=1  --jdk-only over a real
//       JDK: StackTraceElement.toString() is a reviewed Intrinsic rendering the
//       JDK body's text in Rust (rows ste-to-string-*, print-stack-trace-12).
//   CRATONVM_STACK_TRACE_FILL_NAME_FROM_TEXT=1  either mode: the element fill
//       derives each frame's dotted class name from its captured name instead
//       of a by-name class lookup (rows get-stack-trace-12, print-stack-trace-12).
//
// Rows (one untimed warm-up pass, then REPS timed passes; the MINIMUM is
// printed). Every row runs at the bottom of a chain of 12 distinct classes
// (K0 .. K11):
//   ste-to-string-12        toString() of every element of a trace captured
//                           once (15 elements; the fill is not timed;
//                           PRINT_ITERS); checksum counts operations whose
//                           every text equals the getters'
//                           class.method(file:line) (the app loader and the
//                           unnamed module print no prefix), built once
//                           outside the timing
//   ste-to-string-java-base toString() of one java.base element
//                           (ArrayList.forEach, ITERS)
//   get-stack-trace-12      new Exception().getStackTrace(), the fill (ITERS)
//   print-stack-trace-12    printStackTrace(PrintWriter) of a fresh throwable
//                           into a discarding writer (PRINT_ITERS)
//   walker-frames-12        StackWalker.walk over the same frames (PRINT_ITERS), filtered by
//                           StackFrame.getClassName() (no element built)
//   walker-to-ste-12        the same walk mapping every frame through
//                           toStackTraceElement() (L2W75FrameOriginBench's
//                           row); minus walker-frames-12 is what the elements
//                           cost on the walker path
//
// stdout: per row one deterministic "<row> checksum=<n>" line (no "ns/") and
// one timing line "<row> <x> ns/op (min of REPS)". One row only:
// `L2W76StackTraceTextBench <row>`.
//
// A/B: the SAME binary, interleaved, several rounds, medians of the per-round
// minima; --jdk-only (default) JIT and --nojit, and --compatible for the fill:
//   $B --java-home $J/.. -cp out L2W76StackTraceTextBench                                                          # (a) off
//   env CRATONVM_STACK_TRACE_ELEMENT_TO_STRING_INTRINSIC=1 $B --java-home $J/.. -cp out L2W76StackTraceTextBench   # (b) toString on
//   env CRATONVM_STACK_TRACE_FILL_NAME_FROM_TEXT=1 $B --java-home $J/.. -cp out L2W76StackTraceTextBench           # (c) fill on
//   the same three with --nojit; (a) and (c) with --compatible
//   $J/java -cp out L2W76StackTraceTextBench                                                                       # HotSpot
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// ste-to-string-12 checksum=2000
// ste-to-string-java-base checksum=10000
// get-stack-trace-12 checksum=10000
// print-stack-trace-12 checksum=2000
// walker-frames-12 checksum=2000
// walker-to-ste-12 checksum=2000
// HOTSPOT_EXPECTED_END
import java.io.PrintWriter;
import java.io.Writer;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Consumer;
import java.util.function.Function;
import java.util.stream.Stream;

public class L2W76StackTraceTextBench {
    static final int ITERS = 10_000;
    static final int PRINT_ITERS = 2_000;
    static final int REPS = 5;
    static final ArrayList<Integer> ONE = new ArrayList<>(List.of(1));
    static final PrintWriter DISCARD = new PrintWriter(new Writer() {
        @Override
        public void write(char[] cbuf, int off, int len) {
        }

        @Override
        public void flush() {
        }

        @Override
        public void close() {
        }
    });

    static final int TO_STRING = 0;
    static final int TO_STRING_JAVA_BASE = 1;
    static final int GET = 2;
    static final int PRINT = 3;
    static final int WALKER_FRAMES = 4;
    static final int WALKER_TO_STE = 5;

    // Named functions (no lambdas): the K0 frame counted by its class name,
    // without and with the frame's StackTraceElement.
    static class Frames implements Function<Stream<StackWalker.StackFrame>, Long> {
        @Override
        public Long apply(Stream<StackWalker.StackFrame> s) {
            return s.filter(new java.util.function.Predicate<StackWalker.StackFrame>() {
                @Override
                public boolean test(StackWalker.StackFrame f) {
                    return f.getClassName().endsWith("$K0");
                }
            }).count();
        }
    }

    static class Elements implements Function<Stream<StackWalker.StackFrame>, Long> {
        @Override
        public Long apply(Stream<StackWalker.StackFrame> s) {
            return s.map(new Function<StackWalker.StackFrame, StackTraceElement>() {
                @Override
                public StackTraceElement apply(StackWalker.StackFrame f) {
                    return f.toStackTraceElement();
                }
            }).filter(new java.util.function.Predicate<StackTraceElement>() {
                @Override
                public boolean test(StackTraceElement e) {
                    return e.getClassName().endsWith("$K0");
                }
            }).count();
        }
    }

    static final StackWalker WALKER = StackWalker.getInstance();
    static final Frames FRAMES = new Frames();
    static final Elements ELEMENTS = new Elements();

    // The 12 distinct classes every row's frames pass through.
    static class K0 { static long go(int row, int n) { return K1.go(row, n); } }
    static class K1 { static long go(int row, int n) { return K2.go(row, n); } }
    static class K2 { static long go(int row, int n) { return K3.go(row, n); } }
    static class K3 { static long go(int row, int n) { return K4.go(row, n); } }
    static class K4 { static long go(int row, int n) { return K5.go(row, n); } }
    static class K5 { static long go(int row, int n) { return K6.go(row, n); } }
    static class K6 { static long go(int row, int n) { return K7.go(row, n); } }
    static class K7 { static long go(int row, int n) { return K8.go(row, n); } }
    static class K8 { static long go(int row, int n) { return K9.go(row, n); } }
    static class K9 { static long go(int row, int n) { return K10.go(row, n); } }
    static class K10 { static long go(int row, int n) { return K11.go(row, n); } }
    static class K11 { static long go(int row, int n) { return leaf(row, n); } }

    // A named Consumer (no lambda): its accept frame sits under ArrayList.forEach.
    static class Grab implements Consumer<Integer> {
        StackTraceElement forEach;

        @Override
        public void accept(Integer x) {
            StackTraceElement[] st = new Exception().getStackTrace();
            forEach = st[1];
        }
    }

    static StackTraceElement[] kept;
    static String[] expected;
    static StackTraceElement javaBase;

    static long leaf(int row, int n) {
        long c = 0;
        switch (row) {
            case TO_STRING: {
                if (kept == null) {
                    kept = new Exception().getStackTrace();
                    expected = new String[kept.length];
                    for (int k = 0; k < kept.length; k++) {
                        StackTraceElement e = kept[k];
                        expected[k] = e.getClassName() + "." + e.getMethodName() + "("
                            + e.getFileName() + ":" + e.getLineNumber() + ")";
                    }
                }
                for (int i = 0; i < n; i++) {
                    int ok = 0;
                    for (int k = 0; k < kept.length; k++) {
                        if (kept[k].toString().equals(expected[k])) {
                            ok++;
                        }
                    }
                    if (ok == kept.length) {
                        c++;
                    }
                }
                return c;
            }
            case TO_STRING_JAVA_BASE: {
                if (javaBase == null) {
                    Grab grab = new Grab();
                    ONE.forEach(grab);
                    javaBase = grab.forEach;
                }
                for (int i = 0; i < n; i++) {
                    if (javaBase.toString().startsWith("java.base/java.util.ArrayList.forEach(ArrayList.java:")) {
                        c++;
                    }
                }
                return c;
            }
            case GET:
                for (int i = 0; i < n; i++) {
                    StackTraceElement[] st = new Exception().getStackTrace();
                    if (st.length > 12 && st[0].getMethodName().equals("leaf")
                        && st[1].getClassName().endsWith("$K11")) {
                        c++;
                    }
                }
                return c;
            case WALKER_FRAMES:
                for (int i = 0; i < n; i++) {
                    c += WALKER.walk(FRAMES);
                }
                return c;
            case WALKER_TO_STE:
                for (int i = 0; i < n; i++) {
                    c += WALKER.walk(ELEMENTS);
                }
                return c;
            default:
                for (int i = 0; i < n; i++) {
                    new Exception("p").printStackTrace(DISCARD);
                    c++;
                }
                return c;
        }
    }

    static void bench(String name, int row, int n) {
        long checksum = K0.go(row, n);
        long best = Long.MAX_VALUE;
        for (int rep = 0; rep < REPS; rep++) {
            long t0 = System.nanoTime();
            long got = K0.go(row, n);
            long t1 = System.nanoTime();
            best = Math.min(best, t1 - t0);
            if (got != checksum) {
                System.out.println(name + " unstable " + got + " vs " + checksum);
            }
        }
        System.out.println(name + " checksum=" + checksum);
        System.out.printf("%s %.1f ns/op (min of %d)%n", name, (double) best / n, REPS);
    }

    public static void main(String[] args) {
        String only = args.length > 0 ? args[0] : null;
        String[] rows = {
            "ste-to-string-12", "ste-to-string-java-base", "get-stack-trace-12",
            "print-stack-trace-12", "walker-frames-12", "walker-to-ste-12",
        };
        for (int row = 0; row < rows.length; row++) {
            if (only != null && !only.equals(rows[row])) {
                continue;
            }
            bench(rows[row], row, row == GET || row == TO_STRING_JAVA_BASE ? ITERS : PRINT_ITERS);
        }
    }
}
