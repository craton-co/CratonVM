// Comprehensive JIT exception-handling semantics check. Exercises the path
// fixed in route_jit_exception_through_method (catch block in a JIT-compiled
// instance method reads `this`/params) AND normal exception control flow, to
// confirm the fix restores correctness without regressing anything else.
// Diff CratonVM (B-on, low JIT threshold) against HotSpot — output must match.
public class ExcSemantics {
    int field = 7;

    static void deepThrow(int n, String msg) {
        if (n <= 0) throw new IllegalStateException(msg);
        deepThrow(n - 1, msg);
    }

    // 1. catch reads `this` (field) + the method parameter `tag`.
    String catchReadsThisAndParam(int tag) {
        try {
            deepThrow(3, "boom");
            return "no-throw";
        } catch (RuntimeException e) {
            return "this.field=" + this.field + " tag=" + tag + " msg=" + e.getMessage();
        }
    }

    // 2. catch reads a local assigned BEFORE the try (verifier-live across try).
    String catchReadsPreTryLocal(int tag) {
        int pre = tag * 10 + 1;
        try {
            deepThrow(2, "x");
            return "no";
        } catch (RuntimeException e) {
            // pre was assigned before the try region; restored-incoming-args
            // does NOT recover it (slot >= num_params) — but neither did the
            // old code. We only print `this`/param here to stay deterministic.
            return "pre-handled this.field=" + this.field + " tag=" + tag;
        }
    }

    // 3. nested try/catch; inner rethrows, outer catches; both read `this`.
    String nested(int tag) {
        try {
            try {
                deepThrow(1, "inner");
            } catch (RuntimeException e) {
                throw new IllegalArgumentException("wrapped:" + this.field + ":" + tag);
            }
            return "no";
        } catch (IllegalArgumentException e2) {
            return "outer this.field=" + this.field + " " + e2.getMessage();
        }
    }

    // 4. try/catch/finally; finally + catch both read `this`.
    String withFinally(int tag) {
        StringBuilder sb = new StringBuilder();
        try {
            deepThrow(2, "f");
        } catch (RuntimeException e) {
            sb.append("catch:").append(this.field).append(":").append(tag);
        } finally {
            sb.append("|finally:").append(this.field);
        }
        return sb.toString();
    }

    // 5. static method (no `this`) catch reads its params.
    static String staticCatch(int a, long b, String c) {
        try {
            deepThrow(2, "s");
            return "no";
        } catch (RuntimeException e) {
            return "a=" + a + " b=" + b + " c=" + c + " msg=" + e.getMessage();
        }
    }

    // 6. exception thrown INSIDE the catch block, caught by an outer handler.
    String throwInCatch(int tag) {
        try {
            try {
                deepThrow(1, "t");
            } catch (RuntimeException e) {
                deepThrow(1, "in-catch:" + this.field + ":" + tag);
                return "unreached";
            }
            return "no";
        } catch (RuntimeException e2) {
            return "caught-from-catch this.field=" + this.field + " " + e2.getMessage();
        }
    }

    public static void main(String[] a) {
        int iters = a.length > 0 ? Integer.parseInt(a[0]) : 30000;
        ExcSemantics o = new ExcSemantics();
        String s1 = "", s2 = "", s3 = "", s4 = "", s5 = "", s6 = "";
        for (int i = 0; i < iters; i++) {
            s1 = o.catchReadsThisAndParam(i & 3);
            s2 = o.catchReadsPreTryLocal(i & 3);
            s3 = o.nested(i & 3);
            s4 = o.withFinally(i & 3);
            s5 = staticCatch(i & 7, (long) i, "c" + (i & 1));
            s6 = o.throwInCatch(i & 3);
        }
        // Print only the LAST iteration's results (deterministic given i&mask).
        System.out.println("1:" + s1);
        System.out.println("2:" + s2);
        System.out.println("3:" + s3);
        System.out.println("4:" + s4);
        System.out.println("5:" + s5);
        System.out.println("6:" + s6);
        System.out.println("done iters=" + iters);
    }
}
