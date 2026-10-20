// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 74, lane L2 -- what reading a throwable's stack
// trace costs (the StackTraceElement[] the JDK's Throwable.getOurStackTrace()
// builds through StackTraceElement.of), and where a class-loader delegation
// miss's ClassNotFoundException spends its time. Bench for
// docs/internal/fixed-bugs/interpreter-i74-L2-proposal-stack-trace-element-of-intrinsic-20261107-DONE-20261107.md.
//
// Switch (opt-in, default OFF; the same elements either way):
//   CRATONVM_STACK_TRACE_ELEMENT_OF_INTRINSIC=1  under --jdk-only (real JDK),
//       StackTraceElement.of(Object, int) is built natively, computeFormat()
//       answered by the VM, instead of the JDK body's interpreted allocation
//       loop and per-element computeFormat(). --compatible registers nothing.
//
// Rows (ITERS operations each, one untimed warm-up pass, then REPS timed
// passes; the MINIMUM is printed):
//   get-stack-trace       new Exception().getStackTrace() (4 frames, one class)
//   get-stack-trace-deep  the same 50 frames deeper (54 frames, one class)
//   get-stack-trace-again getStackTrace() of ONE throwable again (the cached
//                         array's clone: the floor under the two rows above)
//   ste-to-string         new Exception().getStackTrace()[0].toString()
//   print-stack-trace     printStackTrace(PrintWriter) of a fresh throwable
//                         into a discarding writer (PRINT_ITERS)
//   new-exception-2       new Exception(name) two frames below the loop
//   new-cnfe-2            new ClassNotFoundException(name) there: the CNFE
//                         constructor chain against the row above
//   throw-cnfe-2          the same CNFE thrown there and caught in the loop:
//                         adds the throw and the two-frame unwind
//   load-hit-platform     platform.loadClass("java.sql.Timestamp"): the JDK
//                         loader chain on its found path, no throwable
//   cnfe-platform         platform.loadClass(missing): the whole delegation
//                         miss (wave 73's row); minus throw-cnfe-2 is the
//                         miss path's own Java
//
// stdout: per row one deterministic "<row> checksum=<n>" line (no "ns/") and
// one timing line "<row> <x> ns/op (min of REPS)". One row only:
// `L2W74StackTraceCostBench <row>`.
//
// A/B: the SAME binary, interleaved, several rounds, medians of the per-round
// minima, --jdk-only (default) and --compatible:
//   $B --java-home $J/.. -cp out L2W74StackTraceCostBench                                                 # (a) off
//   env CRATONVM_STACK_TRACE_ELEMENT_OF_INTRINSIC=1 $B --java-home $J/.. -cp out L2W74StackTraceCostBench # (b) on
//   $B --compatible --java-home $J/.. -cp out L2W74StackTraceCostBench                                    # the native floor
//   $J/java -cp out L2W74StackTraceCostBench                                                              # HotSpot
// Expected: under --jdk-only (b) under (a) on get-stack-trace,
// get-stack-trace-deep, ste-to-string and print-stack-trace, towards the
// --compatible numbers; get-stack-trace-again, the new-/throw-/load-/cnfe-
// rows are controls and must not move (no capture path changed).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// get-stack-trace checksum=20000
// get-stack-trace-deep checksum=20000
// get-stack-trace-again checksum=20000
// ste-to-string checksum=20000
// print-stack-trace checksum=5000
// new-exception-2 checksum=20000
// new-cnfe-2 checksum=20000
// throw-cnfe-2 checksum=20000
// load-hit-platform checksum=20000
// cnfe-platform checksum=20000
// HOTSPOT_EXPECTED_END
import java.io.PrintWriter;
import java.io.Writer;

public class L2W74StackTraceCostBench {
    static final int ITERS = 20_000;
    static final int PRINT_ITERS = 5_000;
    static final int REPS = 5;
    static final String[] NAMES = new String[64];
    static ClassLoader platform;
    static Exception kept;
    static Object sink;
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
    static final int GET_DEEP = 1;
    static final int GET_AGAIN = 2;
    static final int TO_STRING = 3;
    static final int PRINT = 4;
    static final int NEW_EXCEPTION = 5;
    static final int NEW_CNFE = 6;
    static final int THROW_CNFE = 7;
    static final int LOAD_HIT = 8;
    static final int CNFE_PLATFORM = 9;

    static long getStackTrace(int n) {
        long c = 0;
        for (int i = 0; i < n; i++) {
            StackTraceElement[] st = new Exception().getStackTrace();
            if (st.length > 0 && st[0].getMethodName().equals("getStackTrace")) {
                c++;
            }
        }
        return c;
    }

    static long atDepth(int depth, int n) {
        if (depth == 0) {
            return getStackTrace(n);
        }
        return atDepth(depth - 1, n);
    }

    static long getAgain(int n) {
        long c = 0;
        for (int i = 0; i < n; i++) {
            StackTraceElement[] st = kept.getStackTrace();
            if (st.length > 0) {
                c++;
            }
        }
        return c;
    }

    static long toStringRow(int n) {
        long c = 0;
        for (int i = 0; i < n; i++) {
            String s = new Exception().getStackTrace()[0].toString();
            if (s.startsWith("L2W74StackTraceCostBench.toStringRow(")) {
                c++;
            }
        }
        return c;
    }

    static long print(int n) {
        long c = 0;
        for (int i = 0; i < n; i++) {
            new Exception("p").printStackTrace(DISCARD);
            c++;
        }
        return c;
    }

    static Exception newException1(int i) {
        return new Exception(NAMES[i & 63]);
    }

    static Exception newException2(int i) {
        return newException1(i);
    }

    static ClassNotFoundException newCnfe1(int i) {
        return new ClassNotFoundException(NAMES[i & 63]);
    }

    static ClassNotFoundException newCnfe2(int i) {
        return newCnfe1(i);
    }

    static void throwCnfe1(int i) throws ClassNotFoundException {
        throw new ClassNotFoundException(NAMES[i & 63]);
    }

    static void throwCnfe2(int i) throws ClassNotFoundException {
        throwCnfe1(i);
    }

    static long newExceptionRow(int n) {
        long c = 0;
        for (int i = 0; i < n; i++) {
            sink = newException2(i);
            c++;
        }
        return c;
    }

    static long newCnfeRow(int n) {
        long c = 0;
        for (int i = 0; i < n; i++) {
            sink = newCnfe2(i);
            c++;
        }
        return c;
    }

    static long throwCnfeRow(int n) {
        long c = 0;
        for (int i = 0; i < n; i++) {
            try {
                throwCnfe2(i);
            } catch (ClassNotFoundException e) {
                sink = e;
                c++;
            }
        }
        return c;
    }

    static long loadHit(int n) throws ClassNotFoundException {
        long c = 0;
        for (int i = 0; i < n; i++) {
            Class<?> k = platform.loadClass("java.sql.Timestamp");
            if (k != null) {
                c++;
            }
        }
        return c;
    }

    static long cnfe(int n) {
        long c = 0;
        for (int i = 0; i < n; i++) {
            try {
                sink = platform.loadClass(NAMES[i & 63]);
            } catch (ClassNotFoundException e) {
                sink = e;
                c++;
            }
        }
        return c;
    }

    static long run(int row, int n) throws Exception {
        switch (row) {
            case GET:
                return getStackTrace(n);
            case GET_DEEP:
                return atDepth(50, n);
            case GET_AGAIN:
                return getAgain(n);
            case TO_STRING:
                return toStringRow(n);
            case PRINT:
                return print(n);
            case NEW_EXCEPTION:
                return newExceptionRow(n);
            case NEW_CNFE:
                return newCnfeRow(n);
            case THROW_CNFE:
                return throwCnfeRow(n);
            case LOAD_HIT:
                return loadHit(n);
            default:
                return cnfe(n);
        }
    }

    static void bench(String name, int row, int n) throws Exception {
        long checksum = run(row, n);
        long best = Long.MAX_VALUE;
        for (int rep = 0; rep < REPS; rep++) {
            long t0 = System.nanoTime();
            long got = run(row, n);
            long t1 = System.nanoTime();
            best = Math.min(best, t1 - t0);
            if (got != checksum) {
                System.out.println(name + " unstable " + got + " vs " + checksum);
            }
        }
        System.out.println(name + " checksum=" + checksum);
        System.out.printf("%s %.1f ns/op (min of %d)%n", name, (double) best / n, REPS);
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < NAMES.length; i++) {
            NAMES[i] = "l2w74.missing.Absent" + i;
        }
        platform = ClassLoader.getPlatformClassLoader();
        kept = new Exception("kept");
        kept.getStackTrace();
        String only = args.length > 0 ? args[0] : null;
        String[] rows = {
            "get-stack-trace", "get-stack-trace-deep", "get-stack-trace-again", "ste-to-string",
            "print-stack-trace", "new-exception-2", "new-cnfe-2", "throw-cnfe-2",
            "load-hit-platform", "cnfe-platform",
        };
        for (int row = 0; row < rows.length; row++) {
            if (only != null && !only.equals(rows[row])) {
                continue;
            }
            bench(rows[row], row, row == PRINT ? PRINT_ITERS : ITERS);
        }
    }
}
