// Probe for docs/known-issues/perf/
// perf-long-counted-loops-lose-the-optimizing-osr-body-20260918.md
//
// Each mode is ONE call with a huge trip count, so only the OSR body is
// measured.  The five shapes are the ones the page's evidence table names.
public class ArithOsr {
    static long mulOnly(long n) {
        long s = 0;
        for (long i = 0; i < n; i++) {
            s += i * 3 + (i >> 1);
        }
        return s;
    }

    static long divOnly(long n) {
        long s = 0;
        for (long i = 0; i < n; i++) {
            s += i * 3 - i / 2;
        }
        return s;
    }

    static long remOnly(long n) {
        long s = 0;
        for (long i = 0; i < n; i++) {
            s += i * 3 + i % 7;
        }
        return s;
    }

    static long fullExpr(long n) {
        long s = 0;
        for (long i = 0; i < n; i++) {
            s += i * 3 - i / 2 + i % 7;
        }
        return s;
    }

    // Single-operand shapes: the accumulator has only ONE non-trivial
    // operand, so the IR tier's RAX/RCX pair suffices and nothing is parked
    // in a home.  They discriminate "the optimizing body is slow because of
    // the home round-trip" from "because of the division sequence".
    static long divAlone(long n) {
        long s = 0;
        for (long i = 0; i < n; i++) {
            s += i / 2;
        }
        return s;
    }

    static long mulAlone(long n) {
        long s = 0;
        for (long i = 0; i < n; i++) {
            s += i * 3;
        }
        return s;
    }

    static long intInduction(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i * 3 - i / 2 + i % 7;
        }
        return s;
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "mul";
        long n = args.length > 1 ? Long.parseLong(args[1]) : 1000000000L;
        long t0 = System.currentTimeMillis();
        long r;
        if (mode.equals("mul")) {
            r = mulOnly(n);
        } else if (mode.equals("div")) {
            r = divOnly(n);
        } else if (mode.equals("rem")) {
            r = remOnly(n);
        } else if (mode.equals("full")) {
            r = fullExpr(n);
        } else if (mode.equals("div1")) {
            r = divAlone(n);
        } else if (mode.equals("mul1")) {
            r = mulAlone(n);
        } else if (mode.equals("int")) {
            r = intInduction((int) n);
        } else {
            throw new IllegalArgumentException(mode);
        }
        long ms = System.currentTimeMillis() - t0;
        System.out.println(mode + " n=" + n + ": " + ms + " ms [" + r + "]");
    }
}
