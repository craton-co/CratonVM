// j27 wave 4 (lane SPLICECAT):
// docs/feature-designs/jit-ir-optimizations.md
//
// Small callees whose body holds an argument-taking javac string concat
// (`makeConcatWithConstants`), each called hot from a loop so the caller is
// compiled with the callee spliced (under CRATONVM_JIT_SPLICE_CONCAT_BRIDGE=1).
// Rows: a concat on the hot path and on a rarely taken (non-throwing) arm; an
// operand whose toString() throws (message and the probe's own frames with
// line numbers); a null operand; char, float, double and UTF16 operands; a
// deopt-shaped branch after the concat (the toString count proves the callee
// was not replayed); and the stack trace of an exception thrown after the
// concat.
//
// Messages are printed with every char outside printable ASCII escaped as
// backslash-u plus four hex digits, so stdout encoding cannot change a line.
//
//   java -cp <dir> J27SpliceConcatRows                  > hs.txt
//   CRATONVM_JIT_SPLICE_CONCAT_BRIDGE=1 cratonvm --cp <dir> J27SpliceConcatRows | diff - hs.txt
//   (both modes, with and without the switch; and --nojit)
// Splice census (the page's recipe):
//   CRATONVM_JIT_SPLICE_CONCAT_BRIDGE=1 CRATONVM_DBG_JITC=1 cratonvm --cp <dir> J27SpliceConcatRows 2>&1 \
//     | grep -E 'splice-concat-row ADMITTED|no constant word'
// Timing (separate `time` line, optional):
//   java -Dj27.timing=true -cp <dir> J27SpliceConcatRows
//
// The expected HotSpot 25 output is in the page.
public class J27SpliceConcatRows {
    static final int WARM = 5_000;
    static final int INNER = 200;
    static long sink;

    static class Bad {
        @Override
        public String toString() {
            throw new IllegalStateException("bad toString");
        }
    }

    static class Counted {
        int n;

        @Override
        public String toString() {
            n++;
            return "C";
        }
    }

    // Hot path: the concat is the callee's whole body.
    static String tag(int v) {
        return "v=" + v;
    }

    // Cold path: a concat on a rarely taken arm that does not throw (so no
    // cold-throw-arm mask removes it).
    static String label(int v) {
        if (v == -7) {
            return "rare=" + v;
        }
        return "ok";
    }

    static String show(Object o, int v) {
        return "o=" + o + ",v=" + v;
    }

    static String mix(char c, float f, double d, char u) {
        return "c=" + c + ",f=" + f + ",d=" + d + ",u=" + u;
    }

    // A branch never taken during warm-up, AFTER the concat: an uncommon trap
    // there must resume past the concat, never re-run it.
    static int afterConcat(Counted c, int v) {
        String s = "c=" + c + ":" + v;
        if (v < 0) {
            return -s.length();
        }
        return s.length();
    }

    static int throwAfter(int v) {
        String s = "t=" + v;
        if (v < 0) {
            throw new IllegalStateException(s);
        }
        return s.length();
    }

    static long tagLoop(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += tag(i & 1023).length();
        }
        return s;
    }

    static String labelLoop(int n, int rareAt) {
        String last = null;
        int ok = 0;
        for (int i = 0; i < n; i++) {
            String r = label(i == rareAt ? -7 : i & 15);
            if (r.length() == 2) {
                ok++;
            } else {
                last = r;
            }
        }
        return last + "/" + ok;
    }

    static String showLoop(Object o, int n) {
        String last = null;
        for (int i = 0; i < n; i++) {
            last = show(o, i & 7);
        }
        return last;
    }

    static String mixLoop(int n, char c, float f, double d, char u) {
        String last = null;
        for (int i = 0; i < n; i++) {
            last = mix(c, f, d, u);
        }
        return last;
    }

    static long afterLoop(Counted c, int n, int negAt) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += afterConcat(c, i == negAt ? -1 : i & 7);
        }
        return s;
    }

    static long throwLoop(int n, int negAt) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += throwAfter(i == negAt ? -9 : i & 7);
        }
        return s;
    }

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    static String esc(String s) {
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c >= 0x20 && c < 0x7f) {
                b.append(c);
            } else {
                b.append(String.format("\\u%04x", (int) c));
            }
        }
        return b.toString();
    }

    static String ex(Throwable t) {
        return t.getClass().getName() + " " + esc(t.getMessage());
    }

    /** The innermost `k` frames of this probe's own classes, `class.method:line`. */
    static String frames(Throwable t, int k) {
        StringBuilder sb = new StringBuilder();
        int seen = 0;
        for (StackTraceElement e : t.getStackTrace()) {
            if (!e.getClassName().startsWith("J27SpliceConcatRows")) {
                continue;
            }
            if (seen++ > 0) {
                sb.append('<');
            }
            sb.append(e.getClassName()).append('.').append(e.getMethodName());
            sb.append(':').append(e.getLineNumber());
            if (seen == k) {
                break;
            }
        }
        return sb.toString();
    }

    static void warm() {
        long s = 0;
        Counted warmCounted = new Counted();
        for (int i = 0; i < WARM; i++) {
            s += tagLoop(INNER);
            s += labelLoop(INNER, -1).length();
            s += showLoop("w", INNER).length();
            s += mixLoop(INNER, 'a', 0.5f, 2.0, 'b').length();
            s += afterLoop(warmCounted, INNER, -1);
            s += throwLoop(INNER, -1);
        }
        sink = s;
    }

    public static void main(String[] args) {
        warm();

        // 1024 cycles of sum(2 + digits(k), k < 1024) = 5034.
        fact("splicecat-hot-sum", tagLoop(1 << 20));
        fact("splicecat-cold", labelLoop(1_000_000, 999_999));

        fact("splicecat-show", showLoop("g", 1_000_000));
        fact("splicecat-null", showLoop(null, 1_000));
        try {
            fact("splicecat-throwing-tostring", showLoop(new Bad(), 1_000));
        } catch (IllegalStateException e) {
            fact("splicecat-throwing-tostring", ex(e));
            fact("splicecat-throwing-tostring-frames", frames(e, 3));
        }

        fact("splicecat-mix", esc(mixLoop(1_000, 'A', 1.0f / 3.0f, 0.1 + 0.2, 'Ω')));
        fact("splicecat-mix-exp", esc(mixLoop(1_000, 'z', 1.0e10f, 1.0e-5, 'é')));
        fact("splicecat-mix-utf16", esc(mixLoop(1_000, 'Ω', -0.0f, Double.NaN, 'q')));

        Counted counted = new Counted();
        fact("splicecat-deopt-sum", afterLoop(counted, 1_000_000, 999_999));
        fact("splicecat-deopt-tostring-count", counted.n);

        try {
            fact("splicecat-throw-after", throwLoop(1_000, 999));
        } catch (IllegalStateException e) {
            fact("splicecat-throw-after", ex(e));
            fact("splicecat-throw-after-frames", frames(e, 2));
        }

        if (Boolean.getBoolean("j27.timing")) {
            int t = 1 << 24;
            long t0 = System.nanoTime();
            long r = tagLoop(t);
            long t1 = System.nanoTime();
            System.out.println("time splicecat-hot " + (t1 - t0) / (double) t + " (" + r + ")");
        }
    }
}
