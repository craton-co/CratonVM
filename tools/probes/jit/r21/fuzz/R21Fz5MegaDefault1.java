// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 16, lane review-fuzz5: megamorphic interface calls that
// land in DEFAULT methods, two levels of default chain, catch-and-rethrow
// inside the default bodies, and stack traces taken through them.
//
// Risk targeted: the megamorphic hashed-vtable stub and its callee-deopt
// service identity (`CRATONVM_JIT_MEGA_STUB_SERVICE_IDENTITY`, wave 13), the
// spliced-default frame labels (`CRATONVM_JIT_SPLICE_DECLARING_CLASS_FRAME`,
// wave 14: an inherited default must be reported as the interface's frame),
// the constant-pool dependency rule of wave 15
// (`inlined_callee_dependency_class`: a late subtype must still withdraw what
// it overrides), and exception handlers inside spliced callees. The call site
// `s.score(k)` sees 8 receiver classes: S0..S5 inherit `Shape.score`, S6 and S7
// inherit `Sub.score`, which calls `Shape.super.score`. `score` calls the
// abstract `area` (megamorphic again) and wraps an ArithmeticException from
// `Shape.check` into an IllegalStateException; `Sub.score` catches that and
// rethrows it unchanged, or wrapped once more on odd keys.
//
// Facts per pass: the checksum of the scores, the number of exceptions of each
// kind, and the hash of their message chain. In the cold pass and the late pass
// one exception of each shape prints its probe frames (class.method:line), so
// the default frames and their order are checked against HotSpot. Only
// explicitly constructed exceptions are printed: their stack traces are full
// on HotSpot in compiled code too.
//
// Passes: `cold`, `hot0..2` (200k calls each, plus an OSR loop), `late` (the
// never-loaded `S8` arrives: it overrides `score` and `name`, a deopt and a
// withdrawal of every splice of `Shape.score` through the site), `rehot0..1`.
//
// Run:
//   javac -d out R21Fz5MegaDefault1.java
//   java -Xmx256m -cp out R21Fz5MegaDefault1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz5MegaDefault1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz5MegaDefault1
//   cratonvm --nojit -Xmx256m -cp out R21Fz5MegaDefault1
//   CRATONVM_JIT_MEGA_STUB_SERVICE_IDENTITY=0 cratonvm ...           # control arm
//   CRATONVM_JIT_SPLICE_DECLARING_CLASS_FRAME=0 cratonvm ...         # control arm
//   CRATONVM_JIT_SP_MEGA_LEAN_STUB_HIT=1 cratonvm ...
// HotSpot expectation: every line identical on every arm; `hot0..2` equal,
// `rehot0..1` equal.

public class R21Fz5MegaDefault1 {
    static int ise;
    static int ae;
    static int other;
    static long msgHash;

    static void frames(String tag, Throwable t) {
        StringBuilder sb = new StringBuilder(tag);
        for (Throwable c = t; c != null; c = c.getCause()) {
            sb.append(" | ").append(c.getClass().getSimpleName()).append(':').append(c.getMessage());
            for (StackTraceElement e : c.getStackTrace()) {
                if (e.getClassName().startsWith("R21Fz5Md")) {
                    sb.append(' ').append(e.getClassName()).append('.').append(e.getMethodName())
                            .append(':').append(e.getLineNumber());
                }
            }
        }
        System.out.println(sb);
    }

    static void note(Throwable t) {
        if (t instanceof IllegalStateException) {
            ise++;
        } else if (t instanceof ArithmeticException) {
            ae++;
        } else {
            other++;
        }
        long h = 17;
        for (Throwable c = t; c != null; c = c.getCause()) {
            h = h * 1_000_003L + String.valueOf(c.getMessage()).hashCode();
        }
        msgHash = msgHash * 31 + h;
    }

    // The megamorphic call site.
    static long drive(R21Fz5MdShape[] arr, int n, boolean show) {
        long sum = 0;
        boolean[] shown = new boolean[4];
        for (int i = 0; i < n; i++) {
            R21Fz5MdShape s = arr[i % arr.length];
            int k = (i * 7) & 1023;
            try {
                sum = sum * 31 + s.score(k);
            } catch (RuntimeException e) {
                note(e);
                int shape = (e.getCause() == null ? 0 : 1) + (e.getCause() != null && e.getCause().getCause() != null ? 1 : 0);
                if (show && !shown[shape]) {
                    shown[shape] = true;
                    frames("  trace" + shape + " " + s.name() + " k=" + k, e);
                }
            }
        }
        return sum;
    }

    // One long call: the same site, OSR-compiled.
    static long osr(R21Fz5MdShape[] arr, int n) {
        long sum = 0;
        int caught = 0;
        for (int i = 0; i < n; i++) {
            try {
                sum += arr[(i >>> 3) % arr.length].score(i & 511);
            } catch (IllegalStateException e) {
                caught++;
            }
        }
        return sum * 31 + caught;
    }

    static void pass(String tag, R21Fz5MdShape[] arr, int n, boolean show) {
        ise = 0;
        ae = 0;
        other = 0;
        msgHash = 0;
        long sum = drive(arr, n, show);
        long o = osr(arr, n * 2);
        System.out.println(tag + " sum=" + sum + " osr=" + o + " ise=" + ise + " ae=" + ae
                + " other=" + other + " msg=" + msgHash);
    }

    public static void main(String[] args) throws Exception {
        R21Fz5MdShape[] arr = {
            new R21Fz5MdS0(), new R21Fz5MdS1(), new R21Fz5MdS2(), new R21Fz5MdS3(),
            new R21Fz5MdS4(), new R21Fz5MdS5(), new R21Fz5MdS6(), new R21Fz5MdS7(),
        };
        pass("cold", arr, 4096, true);
        for (int r = 0; r < 3; r++) {
            pass("hot" + r, arr, 200_000, false);
        }
        // The late receiver, never loaded before.
        R21Fz5MdShape late = (R21Fz5MdShape) Class.forName("R21Fz5MdS8").getDeclaredConstructor().newInstance();
        R21Fz5MdShape[] arr9 = new R21Fz5MdShape[9];
        System.arraycopy(arr, 0, arr9, 0, 8);
        arr9[8] = late;
        pass("late", arr9, 4096, true);
        for (int r = 0; r < 2; r++) {
            pass("rehot" + r, arr9, 200_000, false);
        }
        System.out.println("done");
    }
}

interface R21Fz5MdShape {
    int area(int k);

    default int score(int k) {
        try {
            return check(area(k)) + 1;
        } catch (ArithmeticException e) {
            throw new IllegalStateException("score " + k + " of " + name(), e);
        }
    }

    default String name() {
        return getClass().getSimpleName();
    }

    static int check(int v) {
        if (v < 0) {
            throw new ArithmeticException("neg " + v);
        }
        return v;
    }
}

interface R21Fz5MdSub extends R21Fz5MdShape {
    @Override
    default int score(int k) {
        try {
            return R21Fz5MdShape.super.score(k) * 2;
        } catch (IllegalStateException e) {
            if ((k & 1) == 0) {
                throw e;
            }
            throw new IllegalStateException("sub " + k, e);
        }
    }
}

class R21Fz5MdS0 implements R21Fz5MdShape {
    public int area(int k) {
        return k * 3;
    }
}

class R21Fz5MdS1 implements R21Fz5MdShape {
    public int area(int k) {
        return k * k - 1000;
    }
}

class R21Fz5MdS2 implements R21Fz5MdShape {
    public int area(int k) {
        return Integer.MAX_VALUE - k * 5;
    }
}

class R21Fz5MdS3 implements R21Fz5MdShape {
    public int area(int k) {
        // Overflows for k > 0: negative, so it throws.
        return Integer.MAX_VALUE + k;
    }
}

class R21Fz5MdS4 implements R21Fz5MdShape {
    public int area(int k) {
        return (k & 7) == 7 ? -k : k >> 1;
    }
}

class R21Fz5MdS5 implements R21Fz5MdShape {
    public int area(int k) {
        return k ^ 0x55;
    }

    @Override
    public String name() {
        return "five";
    }
}

class R21Fz5MdS6 implements R21Fz5MdSub {
    public int area(int k) {
        return k % 13 == 0 ? -13 : k + 6;
    }
}

class R21Fz5MdS7 implements R21Fz5MdSub {
    public int area(int k) {
        return k * 1_000_000;
    }
}

class R21Fz5MdS8 implements R21Fz5MdSub {
    public int area(int k) {
        return k - 512;
    }

    @Override
    public int score(int k) {
        try {
            return R21Fz5MdSub.super.score(k) + 3;
        } catch (IllegalStateException e) {
            throw new IllegalStateException("late " + k, e);
        }
    }

    @Override
    public String name() {
        return "eight";
    }
}
