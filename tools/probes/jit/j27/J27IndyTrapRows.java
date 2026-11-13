// j27 wave 4 (lane INDYTRAP):
// docs/feature-designs/native-dispatch-architecture.md
//
// Every method below has a COLD arm that builds its exception message with an
// `invokedynamic` string concat (javac's `makeConcatWithConstants`), and is
// called hot on its other arm first, so the concat site has never been linked
// when the method is compiled. The cold arm then runs for the first time
// inside the compiled body. Rows: the message of each first throw (null, char,
// float, double, a UTF16 char, recipe constants including a literal holding a
// tag unit, which javac moves into a constant), the class-initialisation and
// toString order around a first-run concat (no user bootstrap method), and the
// OSR-door splice shape `Mid.pos` over `Base.pos` of J27OsrSuperRows.
//
// Messages are printed with every char outside printable ASCII escaped as
// backslash-u plus four hex digits, so stdout encoding cannot change a line.
//
//   java -cp <dir> J27IndyTrapRows                  > hs.txt
//   cratonvm --cp <dir> J27IndyTrapRows | diff - hs.txt
//   (both modes; and --nojit)
// Trap census (the page's recipe):
//   CRATONVM_DBG_JITC=1 cratonvm --cp <dir> J27IndyTrapRows 2>&1 | grep -E 'indy (bridge|trap) pc='
// Timing (separate `time` line, optional):
//   java -Dj27.timing=true -cp <dir> J27IndyTrapRows
//
// The expected HotSpot 25 output is in the page (the escapes would be read as
// unicode escapes by javac if written in this comment).
import java.util.ArrayList;
import java.util.List;

public class J27IndyTrapRows {
    static final int WARM = 200_000;
    static final String K = "K"; // a compile-time constant: folded into the recipe
    static final List<String> LOG = new ArrayList<>();
    static long sink;

    static class Lazy {
        static int tag;

        static {
            LOG.add("clinit-Lazy");
            tag = 42;
        }
    }

    static class Noisy {
        @Override
        public String toString() {
            LOG.add("toString");
            return "T";
        }
    }

    static class Base {
        int p;

        Base pos(int v) {
            if (v < 0) throw new IllegalArgumentException("neg: " + v);
            p = v;
            return this;
        }
    }

    static class Mid extends Base {
        @Override
        Mid pos(int v) {
            super.pos(v);
            return this;
        }
    }

    static int negI(int v) {
        if (v < 0) throw new IllegalArgumentException("neg: " + v);
        return v + 1;
    }

    static int nulls(int v, String s, Object o) {
        if (v < 0) throw new IllegalStateException("s=" + s + ";o=" + o);
        return v ^ 1;
    }

    static int chr(int v, char c) {
        if (v < 0) throw new IllegalArgumentException("c=" + c);
        return v + c;
    }

    static int flt(int v, float f, float g) {
        if (v < 0) throw new IllegalArgumentException("f=" + f + "," + g);
        return v + 2;
    }

    static int dbl(int v, double a, double b, double c) {
        if (v < 0) throw new IllegalArgumentException("d=" + a + "," + b + "," + c);
        return v + 3;
    }

    static int utf(int v, char c) {
        if (v < 0) throw new IllegalArgumentException("Ω=" + v + " " + c);
        return v + 4;
    }

    static int konst(int v, long x) {
        if (v < 0) throw new IllegalArgumentException("<\u0001>" + x + "!" + K);
        return v + 5;
    }

    // `Lazy.tag` is read BEFORE the object operand, so the order is the same
    // whether javac stringifies `o` eagerly or the concat does.
    static int order(int v, Object o) {
        if (v < 0) throw new IllegalArgumentException("tag=" + Lazy.tag + " o=" + o);
        return v + 6;
    }

    static long spliceLoop(Mid m, int n, int throwAt) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += m.pos(i == throwAt ? -3 : (i & 7)).p;
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

    /** The three innermost frames, `class.method`. */
    static String frames(Throwable t) {
        StringBuilder sb = new StringBuilder();
        StackTraceElement[] st = t.getStackTrace();
        for (int i = 0; i < 3 && i < st.length; i++) {
            if (i > 0) sb.append('<');
            sb.append(st[i].getClassName()).append('.').append(st[i].getMethodName());
        }
        return sb.toString();
    }

    interface Row {
        void run();
    }

    static void first(String name, Row row) {
        try {
            row.run();
            fact(name, "no exception");
        } catch (RuntimeException e) {
            fact(name, ex(e));
        }
    }

    static void warm() {
        long s = 0;
        Noisy noisy = new Noisy();
        for (int i = 0; i < WARM; i++) {
            int v = i & 1023;
            s += negI(v);
            s += nulls(v, "x", noisy);
            s += chr(v, 'a');
            s += flt(v, 0.5f, 2.0f);
            s += dbl(v, 0.5, 2.0, 4.0);
            s += utf(v, 'b');
            s += konst(v, 9L);
            s += order(v, noisy);
        }
        sink = s;
    }

    public static void main(String[] args) {
        long w = 0;
        for (int i = 0; i < WARM; i++) w += negI(i & 1023);
        // 195 full cycles of sum(k + 1, k < 1024) = 524,800, plus sum(k + 1, k < 320).
        fact("indytrap-warm", w);
        warm();

        first("indytrap-neg", () -> negI(-5));
        first("indytrap-null", () -> nulls(-1, null, null));
        first("indytrap-char", () -> chr(-1, 'A'));
        first("indytrap-float", () -> flt(-1, 1.0f / 3.0f, 1.0e10f));
        first("indytrap-double", () -> dbl(-1, 0.1 + 0.2, -0.0, 1.0e-5));
        first("indytrap-utf16", () -> utf(-1, 'Ω'));
        first("indytrap-const", () -> konst(-1, Long.MIN_VALUE));

        fact("indytrap-order-before", LOG);
        first("indytrap-order", () -> order(-1, new Noisy()));
        fact("indytrap-order-after", LOG);

        Mid m = new Mid();
        // sum(i & 7) over 1,000,000 = 125,000 cycles * 28.
        fact("indytrap-splice-sum", spliceLoop(m, 1_000_000, -1));
        try {
            spliceLoop(m, 1_000_000, 999_999);
            fact("indytrap-splice-throw", "no exception");
        } catch (IllegalArgumentException e) {
            fact("indytrap-splice-throw", ex(e));
            fact("indytrap-splice-frames", frames(e));
        }

        if (Boolean.getBoolean("j27.timing")) {
            int t = 20_000_000;
            long t0 = System.nanoTime();
            long r = spliceLoop(m, t, -1);
            long t1 = System.nanoTime();
            System.out.println("time indytrap-splice " + (t1 - t0) / (double) t + " (" + r + ")");
        }
    }
}
