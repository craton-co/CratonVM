// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 16, lane review-fuzz5: exception-heavy hot loops whose
// callees are small enough to be spliced and that catch, wrap, rethrow, run
// finally blocks and suppress exceptions inside the spliced body.
//
// Risk targeted: exception handlers inside spliced callees (the local-handler
// sites, `JitLocalHandlerSite`, and the splice of a callee with an exception
// table), the deopt-resume of a throw from a nested splice
// (`deopt_resume::resolve_inlined_callee_hinted`), precise rethrow (`throw e`
// of a caught Throwable keeps its original stack trace), try/finally with a
// `return` in the finally (the exception is discarded), try-with-resources
// whose close() throws (a suppressed exception), and a loop counter whose
// bound wraps at Integer.MAX_VALUE. The counts of each outcome are pure
// functions of the inputs.
//
// Stack traces are printed only for EXPLICITLY constructed exceptions, in the
// cold pass and again in the `trace-hot` pass after everything is compiled:
// HotSpot keeps full traces for them in compiled code (the implicit ones may be
// preallocated, OmitStackTraceInFastThrow). Implicit exceptions (`/ by zero`,
// AIOOBE, NPE) are counted by class only, and their messages are printed only
// in the cold pass.
//
// Passes: `cold`, `hot0..2` (300k iterations each, plus one long OSR call),
// `trace-hot`, `late` (a never-loaded `Res` subclass whose close() throws on
// every call: the try-with-resources site deopts), `rehot0..1`.
//
// Run:
//   javac -d out R21Fz5SpliceRethrow1.java
//   java -Xmx256m -cp out R21Fz5SpliceRethrow1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz5SpliceRethrow1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz5SpliceRethrow1
//   cratonvm --nojit -Xmx256m -cp out R21Fz5SpliceRethrow1
//   CRATONVM_JIT_SPLICE_DECLARING_CLASS_FRAME=0 cratonvm ...           # control arm
// HotSpot expectation: every line identical on every arm; `hot0..2` equal,
// `rehot0..1` equal; the `trace-hot` lines carry the same callee frames as
// the matching `cold` trace lines (only the driver frame, `run` versus
// `traces`, differs).

public class R21Fz5SpliceRethrow1 {
    static int[] counts = new int[12];
    static long hash;
    static int finallies;

    static final String[] NAMES = {
        "ok", "wrapped", "rethrown", "finallyRet", "suppressed", "div0", "aioobe", "npe",
        "other", "closeOnly", "nested", "unused",
    };

    static void reset() {
        java.util.Arrays.fill(counts, 0);
        hash = 0;
        finallies = 0;
    }

    static String summary() {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < counts.length; i++) {
            if (counts[i] != 0) {
                sb.append(' ').append(NAMES[i]).append('=').append(counts[i]);
            }
        }
        return sb.append(" fin=").append(finallies).append(" h=").append(hash).toString();
    }

    static boolean isImplicit(Throwable t) {
        return t instanceof ArithmeticException
                || t instanceof ArrayIndexOutOfBoundsException
                || t instanceof NullPointerException;
    }

    static void trace(String tag, Throwable t) {
        StringBuilder sb = new StringBuilder("  ").append(tag);
        for (Throwable c = t; c != null; c = c.getCause()) {
            sb.append(" | ").append(c.getClass().getSimpleName());
            if (isImplicit(c)) {
                // HotSpot may hand hot code a preallocated one: no frames.
                continue;
            }
            for (StackTraceElement e : c.getStackTrace()) {
                if (e.getClassName().startsWith("R21Fz5Sr")) {
                    sb.append(' ').append(e.getMethodName()).append(':').append(e.getLineNumber());
                }
            }
            for (Throwable s : c.getSuppressed()) {
                sb.append(" [suppressed ").append(s.getClass().getSimpleName()).append(':')
                        .append(s.getMessage()).append(']');
            }
        }
        System.out.println(sb);
    }

    public static void main(String[] args) throws Exception {
        int[] data = new int[64];
        for (int i = 0; i < data.length; i++) {
            data[i] = (i * 37) % 11 - 3;
        }
        R21Fz5SrRes plain = new R21Fz5SrRes();

        reset();
        R21Fz5SrKernels.run(data, plain, 4096, true, true);
        System.out.println("cold" + summary());
        for (int r = 0; r < 3; r++) {
            reset();
            R21Fz5SrKernels.run(data, plain, 300_000, false, false);
            long o = R21Fz5SrKernels.osr(data, 2_000_000);
            System.out.println("hot" + r + summary() + " osr=" + o);
        }
        reset();
        R21Fz5SrKernels.traces(data, plain, "trace-hot");

        R21Fz5SrRes late = (R21Fz5SrRes) Class.forName("R21Fz5SrBadRes").getDeclaredConstructor().newInstance();
        reset();
        R21Fz5SrKernels.run(data, late, 4096, true, false);
        System.out.println("late" + summary());
        for (int r = 0; r < 2; r++) {
            reset();
            R21Fz5SrKernels.run(data, late, 300_000, false, false);
            long o = R21Fz5SrKernels.osr(data, 2_000_000);
            System.out.println("rehot" + r + summary() + " osr=" + o);
        }
        System.out.println("done");
    }
}

class R21Fz5SrRes implements AutoCloseable {
    int opened;

    R21Fz5SrRes open() {
        opened++;
        return this;
    }

    @Override
    public void close() {
        // the plain resource closes quietly
    }
}

class R21Fz5SrBadRes extends R21Fz5SrRes {
    @Override
    public void close() {
        throw new IllegalStateException("close " + opened % 7);
    }
}

class R21Fz5SrKernels {
    static int leaf(int[] a, int i) {
        int d = a[i & 63];
        // `/ by zero` when d == 0; AIOOBE when i is negative and odd-shaped.
        return 1000 / d + a[(i % 70)];
    }

    // Catch an implicit exception and wrap it: always an explicit throw.
    static int wrap(int[] a, int i) {
        try {
            return leaf(a, i);
        } catch (ArithmeticException e) {
            throw new IllegalArgumentException("wrap " + (i & 63), e);
        }
    }

    // Precise rethrow: the caught exception goes on unchanged.
    static int rethrow(int[] a, int i) {
        try {
            return wrap(a, i);
        } catch (Throwable t) {
            R21Fz5SpliceRethrow1.finallies += 0;
            throw t;
        } finally {
            R21Fz5SpliceRethrow1.finallies++;
        }
    }

    // A return in finally discards the exception.
    @SuppressWarnings("finally")
    static int finallyReturn(int[] a, int i) {
        try {
            return rethrow(a, i);
        } finally {
            if ((i & 15) == 3) {
                return -7;
            }
        }
    }

    // try-with-resources: close() may throw, alone or suppressed.
    static int withRes(R21Fz5SrRes res, int[] a, int i) {
        try (R21Fz5SrRes r = res.open()) {
            if ((i & 31) == 9) {
                throw new UnsupportedOperationException("body " + (i & 31));
            }
            return a[i & 63];
        }
    }

    static int nested(int[] a, int i) {
        try {
            try {
                return finallyReturn(a, i);
            } catch (IllegalArgumentException e) {
                if ((i & 1) == 0) {
                    throw new IllegalStateException("nested " + (i & 63), e);
                }
                return -1;
            }
        } catch (IllegalStateException e) {
            R21Fz5SpliceRethrow1.counts[10]++;
            throw e;
        }
    }

    static void record(Throwable t, int i) {
        int[] c = R21Fz5SpliceRethrow1.counts;
        if (t instanceof ArrayIndexOutOfBoundsException) {
            c[6]++;
        } else if (t instanceof ArithmeticException) {
            c[5]++;
        } else if (t instanceof NullPointerException) {
            c[7]++;
        } else if (t instanceof IllegalStateException && t.getCause() != null) {
            c[1]++;
        } else if (t instanceof IllegalArgumentException) {
            c[2]++;
        } else if (t instanceof UnsupportedOperationException) {
            c[t.getSuppressed().length > 0 ? 4 : 8]++;
        } else if (t instanceof IllegalStateException) {
            c[9]++;
        } else {
            c[8]++;
        }
        R21Fz5SpliceRethrow1.hash = R21Fz5SpliceRethrow1.hash * 31 + t.getClass().getName().hashCode() + i;
    }

    static void run(int[] a, R21Fz5SrRes res, int n, boolean cold, boolean implicitText) {
        boolean[] shown = new boolean[12];
        int[] nul = (n < 0) ? a : null;
        // The loop index wraps: it starts 2000 below MAX_VALUE and runs n trips,
        // so most of the trips see negative i (the `% 70` index is then negative).
        int i = Integer.MAX_VALUE - 2000;
        for (int t = 0; t < n; t++, i++) {
            int kind = t % 5;
            try {
                int v;
                switch (kind) {
                    case 0 -> v = nested(a, i);
                    case 1 -> v = withRes(res, a, i);
                    case 2 -> v = rethrow(a, i);
                    case 3 -> v = (t & 1023) == 1023 ? nul[0] : finallyReturn(a, i);
                    default -> v = wrap(a, i);
                }
                R21Fz5SpliceRethrow1.counts[0]++;
                R21Fz5SpliceRethrow1.hash = R21Fz5SpliceRethrow1.hash * 31 + v;
            } catch (RuntimeException e) {
                record(e, kind);
                if (cold) {
                    int slot = Math.min(kind * 2 + (e.getCause() == null ? 0 : 1), 11);
                    if (!shown[slot]) {
                        shown[slot] = true;
                        if (R21Fz5SpliceRethrow1.isImplicit(e)) {
                            // The message only in the first pass: later passes run hot
                            // code, where HotSpot may throw a preallocated one.
                            System.out.println("  implicit k" + kind + " " + e.getClass().getSimpleName()
                                    + (implicitText && !(e instanceof NullPointerException)
                                            ? ": " + e.getMessage() : ""));
                        } else {
                            R21Fz5SpliceRethrow1.trace("k" + kind, e);
                        }
                    }
                }
            }
        }
    }

    // The traces of the explicit shapes, once everything is hot.
    static void traces(int[] a, R21Fz5SrRes res, String tag) {
        System.out.println(tag);
        int[] picks = {0, 1, 2, 4};
        for (int kind : picks) {
            for (int t = 0; t < 4096; t++) {
                int i = Integer.MAX_VALUE - 2000 + t;
                try {
                    switch (kind) {
                        case 0 -> nested(a, i);
                        case 1 -> withRes(res, a, i);
                        case 2 -> rethrow(a, i);
                        default -> wrap(a, i);
                    }
                } catch (RuntimeException e) {
                    if (!(e instanceof ArithmeticException || e instanceof ArrayIndexOutOfBoundsException)) {
                        R21Fz5SpliceRethrow1.trace("k" + kind, e);
                        break;
                    }
                }
            }
        }
    }

    // One long call: wrap/rethrow inside an OSR-compiled loop.
    static long osr(int[] a, int n) {
        long h = 0;
        int caught = 0;
        for (int i = 0; i < n; i++) {
            try {
                h += rethrow(a, i);
            } catch (IllegalArgumentException e) {
                caught++;
            } catch (ArrayIndexOutOfBoundsException e) {
                caught += 2;
            }
        }
        return h * 31 + caught;
    }
}
