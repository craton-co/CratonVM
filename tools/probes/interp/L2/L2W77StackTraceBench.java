// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 77, lane L2 -- what a walked frame's element and a
// repeated getStackTrace() cost, on frames through 12 distinct classes.
// Bench for docs/internal/fixed-bugs/interpreter-i76-L2-proposal-walker-element-of-stack-frame-info-20261109-DONE-20261110.md
// and docs/internal/fixed-bugs/interpreter-i76-L2-proposal-compatible-get-stack-trace-keeps-the-built-array-20261109-DONE-20261110.md.
//
// Switches (opt-in, default OFF):
//   CRATONVM_STACK_TRACE_ELEMENT_OF_FRAME_INTRINSIC=1  --jdk-only over a real
//       JDK: StackTraceElement.of(StackFrameInfo) is a reviewed Intrinsic
//       (rows walker-*; walker-to-ste-12 minus walker-frames-12 is the
//       elements' share).
//   CRATONVM_COMPAT_STACK_TRACE_KEEPS_ARRAY=1  --compatible: the native
//       Throwable.getStackTrace() keeps the array it built and answers copies
//       (rows get-*; print-after-get-12 is the cost side: the printer then
//       renders the kept elements, as HotSpot's printStackTrace does).
//
// Rows (one untimed warm-up pass, then REPS timed passes; the MINIMUM is
// printed). Every row runs at the bottom of a chain of 12 distinct classes
// (K0 .. K11):
//   walker-frames-12       StackWalker.walk over the frames, filtered by
//                          StackFrame.getClassName() (no element built; ITERS_WALK)
//   walker-to-ste-12       the same walk mapping every frame through
//                          toStackTraceElement() (ITERS_WALK)
//   get-stack-trace-again  kept.getStackTrace() of one throwable captured once
//                          (ITERS)
//   get-stack-trace-twice  e.getStackTrace() twice on a fresh throwable (ITERS)
//   print-after-get-12     getStackTrace() then printStackTrace(PrintWriter) of
//                          a fresh throwable into a discarding writer (ITERS_WALK)
//
// stdout: per row one deterministic "<row> checksum=<n>" line (no "ns/") and
// one timing line "<row> <x> ns/op (min of REPS)". One row only:
// `L2W77StackTraceBench <row>`.
//
// A/B: the SAME binary, interleaved, several rounds, medians of the per-round
// minima:
//   $B --java-home $J/.. -cp out L2W77StackTraceBench                                                      # (a) jdk-only off
//   env CRATONVM_STACK_TRACE_ELEMENT_OF_FRAME_INTRINSIC=1 $B --java-home $J/.. -cp out L2W77StackTraceBench # (b) jdk-only on
//   (a) and (b) with --nojit
//   $B --compatible --java-home $J/.. -cp out L2W77StackTraceBench                                         # (c) compat off
//   env CRATONVM_COMPAT_STACK_TRACE_KEEPS_ARRAY=1 $B --compatible --java-home $J/.. -cp out L2W77StackTraceBench  # (d) compat on
//   $J/java -cp out L2W77StackTraceBench                                                                   # HotSpot
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// walker-frames-12 checksum=2000
// walker-to-ste-12 checksum=2000
// get-stack-trace-again checksum=10000
// get-stack-trace-twice checksum=10000
// print-after-get-12 checksum=2000
// HOTSPOT_EXPECTED_END
import java.io.PrintWriter;
import java.io.Writer;
import java.util.function.Function;
import java.util.function.Predicate;
import java.util.stream.Stream;

public class L2W77StackTraceBench {
    static final int ITERS = 10_000;
    static final int ITERS_WALK = 2_000;
    static final int REPS = 5;
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

    static final int WALKER_FRAMES = 0;
    static final int WALKER_TO_STE = 1;
    static final int GET_AGAIN = 2;
    static final int GET_TWICE = 3;
    static final int PRINT_AFTER_GET = 4;

    // Named functions (no lambdas): the K0 frame counted by its class name,
    // without and with the frame's StackTraceElement.
    static class Frames implements Function<Stream<StackWalker.StackFrame>, Long> {
        @Override
        public Long apply(Stream<StackWalker.StackFrame> s) {
            return s.filter(new Predicate<StackWalker.StackFrame>() {
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
            }).filter(new Predicate<StackTraceElement>() {
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

    static Throwable kept;

    static boolean looksRight(StackTraceElement[] st) {
        return st.length > 12 && st[0].getMethodName().equals("leaf")
            && st[1].getClassName().endsWith("$K11");
    }

    static long leaf(int row, int n) {
        long c = 0;
        switch (row) {
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
            case GET_AGAIN:
                if (kept == null) {
                    kept = new Exception("kept");
                }
                for (int i = 0; i < n; i++) {
                    if (looksRight(kept.getStackTrace())) {
                        c++;
                    }
                }
                return c;
            case GET_TWICE:
                for (int i = 0; i < n; i++) {
                    Exception e = new Exception();
                    StackTraceElement[] a = e.getStackTrace();
                    StackTraceElement[] b = e.getStackTrace();
                    if (a != b && looksRight(b)) {
                        c++;
                    }
                }
                return c;
            default:
                for (int i = 0; i < n; i++) {
                    Exception e = new Exception("p");
                    if (looksRight(e.getStackTrace())) {
                        e.printStackTrace(DISCARD);
                        c++;
                    }
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
            "walker-frames-12", "walker-to-ste-12", "get-stack-trace-again",
            "get-stack-trace-twice", "print-after-get-12",
        };
        for (int row = 0; row < rows.length; row++) {
            if (only != null && !only.equals(rows[row])) {
                continue;
            }
            int n = row == GET_AGAIN || row == GET_TWICE ? ITERS : ITERS_WALK;
            bench(rows[row], row, n);
        }
    }
}
