// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 75, lane L2 -- what materialising StackTraceElements
// costs when a trace spans MANY classes (framework stacks: 10-30), the case a
// per-VM frame-origin record pays off on. Bench for
// docs/internal/fixed-bugs/interpreter-i74-L2-proposal-per-vm-frame-origin-record-20261107-DONE-20261108.md.
//
// Switch (opt-in, default OFF; the same elements either way):
//   CRATONVM_STACK_TRACE_ORIGIN_RECORD=1  a StackTraceElement the VM fills for
//       a class it filled one for before (any array, either mode) copies the
//       class's origin fields and interned name from a per-VM record; under
//       --jdk-only the StackTraceElement.of intrinsic also reuses the class's
//       computeFormat() answer.
//
// Rows (ITERS operations each, one untimed warm-up pass, then REPS timed
// passes; the MINIMUM is printed). Every row runs at the bottom of a chain of
// 12 distinct classes (K0 .. K11), so each trace has 13+ distinct classes:
//   get-stack-trace-12       new Exception().getStackTrace()
//   java-base-trace-12       the same, captured inside ArrayList.forEach's
//                            callback (a java.base frame in the trace)
//   print-stack-trace-12     printStackTrace(PrintWriter) of a fresh throwable
//                            into a discarding writer (PRINT_ITERS)
//   thread-stack-trace-12    Thread.currentThread().getStackTrace()
//   walker-to-ste-12         StackWalker: every frame's toStackTraceElement()
//   new-exception-12         new Exception() only: the capture, no element is
//                            filled (control: must not move)
//
// stdout: per row one deterministic "<row> checksum=<n>" line (no "ns/") and
// one timing line "<row> <x> ns/op (min of REPS)". One row only:
// `L2W75FrameOriginBench <row>`.
//
// A/B: the SAME binary, interleaved, several rounds, medians of the per-round
// minima, --jdk-only (default) and --compatible, JIT and --nojit:
//   $B --java-home $J/.. -cp out L2W75FrameOriginBench                                          # (a) off
//   env CRATONVM_STACK_TRACE_ORIGIN_RECORD=1 $B --java-home $J/.. -cp out L2W75FrameOriginBench # (b) on
//   $B --compatible --java-home $J/.. -cp out L2W75FrameOriginBench                             # (a) off
//   env CRATONVM_STACK_TRACE_ORIGIN_RECORD=1 $B --compatible --java-home $J/.. -cp out L2W75FrameOriginBench
//   $J/java -cp out L2W75FrameOriginBench                                                       # HotSpot
// Also the wave-74 bench L2W74StackTraceCostBench, (a) against (b): its
// one-class rows must not get slower (the off state and the one-class hit).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// get-stack-trace-12 checksum=10000
// java-base-trace-12 checksum=10000
// print-stack-trace-12 checksum=2000
// thread-stack-trace-12 checksum=10000
// walker-to-ste-12 checksum=10000
// new-exception-12 checksum=10000
// HOTSPOT_EXPECTED_END
import java.io.PrintWriter;
import java.io.Writer;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Consumer;
import java.util.function.Function;
import java.util.stream.Stream;

public class L2W75FrameOriginBench {
    static final int ITERS = 10_000;
    static final int PRINT_ITERS = 2_000;
    static final int REPS = 5;
    static Object sink;
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

    static final int GET = 0;
    static final int JAVA_BASE = 1;
    static final int PRINT = 2;
    static final int THREAD = 3;
    static final int WALKER = 4;
    static final int NEW_EXCEPTION = 5;

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
        long count;

        @Override
        public void accept(Integer x) {
            StackTraceElement[] st = new Exception().getStackTrace();
            if (st.length > 2 && st[1].getMethodName().equals("forEach")) {
                count++;
            }
        }
    }

    static class Walk implements Function<Stream<StackWalker.StackFrame>, Long> {
        @Override
        public Long apply(Stream<StackWalker.StackFrame> s) {
            return s.map(StackWalker.StackFrame::toStackTraceElement)
                .filter(e -> e.getClassName().endsWith("$K0"))
                .count();
        }
    }

    static final StackWalker WALKER_INSTANCE = StackWalker.getInstance();
    static final Walk WALK = new Walk();

    static long leaf(int row, int n) {
        long c = 0;
        switch (row) {
            case GET:
                for (int i = 0; i < n; i++) {
                    StackTraceElement[] st = new Exception().getStackTrace();
                    if (st.length > 12 && st[0].getMethodName().equals("leaf")) {
                        c++;
                    }
                }
                return c;
            case JAVA_BASE: {
                Grab grab = new Grab();
                for (int i = 0; i < n; i++) {
                    ONE.forEach(grab);
                }
                return grab.count;
            }
            case PRINT:
                for (int i = 0; i < n; i++) {
                    new Exception("p").printStackTrace(DISCARD);
                    c++;
                }
                return c;
            case THREAD:
                for (int i = 0; i < n; i++) {
                    StackTraceElement[] st = Thread.currentThread().getStackTrace();
                    if (st.length > 12) {
                        c++;
                    }
                }
                return c;
            case WALKER:
                for (int i = 0; i < n; i++) {
                    c += WALKER_INSTANCE.walk(WALK);
                }
                return c;
            default:
                for (int i = 0; i < n; i++) {
                    sink = new Exception();
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
            "get-stack-trace-12", "java-base-trace-12", "print-stack-trace-12",
            "thread-stack-trace-12", "walker-to-ste-12", "new-exception-12",
        };
        for (int row = 0; row < rows.length; row++) {
            if (only != null && !only.equals(rows[row])) {
                continue;
            }
            bench(rows[row], row, row == PRINT ? PRINT_ITERS : ITERS);
        }
    }
}
