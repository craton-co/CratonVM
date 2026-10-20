import java.lang.reflect.Array;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

/**
 * Round nio21 wave 3, lane L2: a CALL inside a {@code try} whose callee throws, caught by a handler
 * of the calling method, in every shape the optimizing tier's call-site exception edges ({@code
 * CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES}, default OFF) take or decline, so the switch-on arm can
 * be diffed against HotSpot 25 and against the switch-off arm.
 *
 * <p>Each {@code conf} line runs its shape {@code N} times in a loop (so the caller is hot and
 * compiled when the line's last iterations run) and prints a digest that depends on which handler
 * ran, the locals the handler saw, and the exception's class (and, where HotSpot's text is plain
 * Java, its message).
 *
 * <ul>
 *   <li>{@code subclass}   -- the callee throws {@code IllegalStateException}, the catch names
 *       {@code RuntimeException}: decided by the runtime matcher, not at compile time.</li>
 *   <li>{@code second}     -- two clauses over one call, the second one matches.</li>
 *   <li>{@code finally}    -- a catch-all ({@code finally}) entered from the call, which then
 *       rethrows to the caller; the counter shows it ran exactly once per throw.</li>
 *   <li>{@code nested}     -- inner {@code try} does not match, the outer one does.</li>
 *   <li>{@code rethrow}    -- the handler throws a new exception, caught by the caller.</li>
 *   <li>{@code nomatch}    -- no row of the caller matches: the exception leaves it.</li>
 *   <li>{@code locals}     -- the handler reads locals written inside the {@code try} before the
 *       call.</li>
 *   <li>{@code loop}       -- the call and its catch inside a loop body ({@code ite}'s shape).</li>
 *   <li>{@code reflect}    -- {@code Method.invoke} of a throwing method, caught as {@code
 *       InvocationTargetException} (the {@code ite} row itself).</li>
 *   <li>{@code native}     -- exceptions raised by natives ({@code Array.newInstance}, {@code
 *       Object.clone}, {@code System.arraycopy}).</li>
 *   <li>{@code deopt}      -- the callee deopts on an unseen argument class and then throws.</li>
 *   <li>{@code ctor}       -- an {@code invokespecial} constructor that throws.</li>
 *   <li>{@code iface}      -- an {@code invokeinterface} that throws.</li>
 *   <li>{@code wide}       -- {@code long}/{@code double} callees that sometimes throw and sometimes
 *       return {@code Long.MIN_VALUE} / {@code -0.0}, whose bits are the JIT's exception sentinel.</li>
 *   <li>{@code error}      -- an {@code Error} caught by {@code catch (Throwable)}.</li>
 * </ul>
 *
 * <pre>
 *   javac -d /tmp/p21 tools/probes/Nio21CallThrow.java
 *   java     -cp /tmp/p21 Nio21CallThrow                                    # HotSpot reference
 *   cratonvm -cp /tmp/p21 Nio21CallThrow                                    # --jdk-only, edges off
 *   CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES=1 cratonvm -cp /tmp/p21 Nio21CallThrow
 *   CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES=1 cratonvm --compatible -cp /tmp/p21 Nio21CallThrow
 *   # engagement: armed sites and pads per compile
 *   CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES=1 CRATONVM_DBG_JITC=1 cratonvm -cp /tmp/p21 \
 *       Nio21CallThrow 2>&amp;1 | grep -E 'ir-call-throw'
 *   # bench rows (ns/op), interleave arms and take medians:
 *   CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES=1 cratonvm -Dnio21.rows=bench -cp /tmp/p21 Nio21CallThrow
 * </pre>
 *
 * {@code -Dnio21.n=} sets the loop count per conf line (default 20 000). With {@code
 * -Dnio21.rows=bench} the conf lines are still printed first, then one {@code <row> <N> ns/op} line
 * per row.
 */
public class Nio21CallThrow {
    static final int N = Integer.getInteger("nio21.n", 20_000);
    static final String ROWS = System.getProperty("nio21.rows", "conf");

    static int sink;

    // ---- callees ----

    static int thrower(int i) {
        if ((i & 3) != 3) {
            throw new IllegalStateException("ise" + (i & 1));
        }
        return i;
    }

    static int argThrower(int i) {
        if ((i & 1) == 0) {
            throw new IllegalArgumentException("iae" + (i & 7));
        }
        return i * 2;
    }

    static int arithThrower(int i) {
        if ((i & 1) == 0) {
            throw new ArithmeticException("arith");
        }
        return i + 1;
    }

    static int unsupported(int i) {
        if ((i & 1) == 0) {
            throw new UnsupportedOperationException("uoe");
        }
        return i;
    }

    // ---- 1. subclass catch ----
    static int subclass(int i) {
        try {
            return thrower(i);
        } catch (RuntimeException e) {
            return 1000 + e.getMessage().length() + (e instanceof IllegalStateException ? 10 : 0);
        }
    }

    // ---- 2. second clause ----
    static int second(int i) {
        try {
            return argThrower(i);
        } catch (IllegalStateException e) {
            return -3;
        } catch (IllegalArgumentException e) {
            return 2000 + e.getMessage().length() + (i & 1);
        }
    }

    // ---- 3. finally entered from the call, then rethrow to the caller ----
    static int finallyCount;

    static int finallyInner(int i) {
        try {
            return unsupported(i);
        } finally {
            finallyCount++;
        }
    }

    static int finallyOuter(int i) {
        try {
            return finallyInner(i);
        } catch (UnsupportedOperationException e) {
            return 3000 + e.getMessage().length();
        }
    }

    // ---- 4. nested try ----
    static int nested(int i) {
        int x = i & 7;
        try {
            try {
                x += 1;
                return arithThrower(i) + x;
            } catch (IllegalStateException e) {
                return -100;
            }
        } catch (ArithmeticException e) {
            return 4000 + x;
        }
    }

    // ---- 5. rethrow from the handler ----
    static int rethrowInner(int i) {
        try {
            return thrower(i);
        } catch (IllegalStateException e) {
            throw new IllegalArgumentException("outer:" + e.getMessage(), e);
        }
    }

    static int rethrow(int i) {
        try {
            return rethrowInner(i);
        } catch (IllegalArgumentException e) {
            return 5000 + e.getMessage().length()
                    + (e.getCause() instanceof IllegalStateException ? 1 : 0)
                    + (e.getStackTrace()[0].getMethodName().equals("rethrowInner") ? 100 : 0);
        }
    }

    // ---- 6. no matching row: the exception leaves the caller ----
    static int nomatchInner(int i) {
        try {
            return argThrower(i);
        } catch (IllegalStateException e) {
            return -6;
        }
    }

    static int nomatch(int i) {
        try {
            return nomatchInner(i);
        } catch (IllegalArgumentException e) {
            return 6000 + e.getMessage().length()
                    + (e.getStackTrace()[0].getMethodName().equals("argThrower") ? 100 : 0);
        }
    }

    // ---- 7. locals written in the try before the call ----
    static int locals(int i) {
        int a = i;
        long b = 7L * i;
        String s = "s";
        try {
            a += 3;
            b ^= 0x55L;
            s = s + (i & 3);
            return argThrower(i) + a;
        } catch (IllegalArgumentException e) {
            return 7000 + a + (int) (b & 0xff) + s.length();
        }
    }

    // ---- 8. call and catch inside a loop ----
    static int loop(int i) {
        int h = 0;
        for (int r = 0; r < 4; r++) {
            try {
                h += thrower(i + r);
            } catch (IllegalStateException e) {
                h += e.getMessage().length() + r;
            }
        }
        return h;
    }

    // ---- 9. Method.invoke (the ite row) ----
    public static int reflectTarget(int i) {
        return thrower(i);
    }

    static final Method TARGET;

    static {
        try {
            TARGET = Nio21CallThrow.class.getMethod("reflectTarget", int.class);
        } catch (NoSuchMethodException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static int reflect(int i) {
        try {
            return (Integer) TARGET.invoke(null, i);
        } catch (InvocationTargetException e) {
            return 9000 + e.getCause().getMessage().length();
        } catch (IllegalAccessException e) {
            return -9;
        }
    }

    // ---- 10. exceptions from natives ----
    static final class NoClone {
        Object copy() throws CloneNotSupportedException {
            return clone();
        }
    }

    static final NoClone NO_CLONE = new NoClone();
    static final int[] SRC = new int[8];
    static final int[] DST = new int[8];

    static int natives(int i) {
        int h = 0;
        try {
            h += Array.getLength(Array.newInstance(int.class, (i & 1) == 0 ? -1 : 2));
        } catch (NegativeArraySizeException e) {
            h += 10;
        }
        try {
            h += NO_CLONE.copy() == null ? 1 : 2;
        } catch (CloneNotSupportedException e) {
            h += 100 + e.getClass().getSimpleName().length();
        }
        try {
            System.arraycopy(SRC, 0, DST, (i & 1) == 0 ? 6 : 0, 4);
            h += 1;
        } catch (IndexOutOfBoundsException e) {
            h += 1000 + (e instanceof ArrayIndexOutOfBoundsException ? 1 : 0);
        }
        return h;
    }

    // ---- 11. the callee deopts on an unseen class, then throws ----
    static final Object[] BOXES = new Object[64];

    static {
        for (int k = 0; k < BOXES.length; k++) {
            BOXES[k] = Integer.valueOf(k);
        }
    }

    static int deoptCallee(Object o, int i) {
        if (o instanceof Integer n) {
            return n + i;
        }
        throw new IllegalArgumentException("not an int: " + o.getClass().getSimpleName());
    }

    static int deopt(int i) {
        Object o = (i % 4096 == 4095) ? (Object) ("s" + i) : BOXES[i & 63];
        try {
            return deoptCallee(o, i) & 0xffff;
        } catch (IllegalArgumentException e) {
            return 11000 + e.getMessage().length();
        }
    }

    // ---- 12. a throwing constructor ----
    static final class Picky {
        final int v;

        Picky(int v) {
            if ((v & 1) == 0) {
                throw new IllegalStateException("even" + (v & 3));
            }
            this.v = v;
        }
    }

    static int ctor(int i) {
        try {
            return new Picky(i).v;
        } catch (IllegalStateException e) {
            return 12000 + e.getMessage().length();
        }
    }

    // ---- 13. an interface call ----
    interface Shape {
        int area(int i);
    }

    static final class Bad implements Shape {
        public int area(int i) {
            if ((i & 1) == 0) {
                throw new UnsupportedOperationException("bad" + (i & 3));
            }
            return i;
        }
    }

    static final class Good implements Shape {
        public int area(int i) {
            return i * i;
        }
    }

    static final Shape[] SHAPES = {new Bad(), new Good(), new Bad()};

    static int iface(int i) {
        Shape s = SHAPES[i % 3];
        try {
            return s.area(i) & 0xfff;
        } catch (UnsupportedOperationException e) {
            return 13000 + e.getMessage().length();
        }
    }

    // ---- 14. long / double results that collide with the sentinel ----
    static long wideLong(int i) {
        switch (i & 3) {
            case 0:
                return Long.MIN_VALUE;
            case 1:
                throw new IllegalStateException("long");
            default:
                return i;
        }
    }

    static double wideDouble(int i) {
        switch (i & 3) {
            case 0:
                return -0.0;
            case 1:
                throw new IllegalStateException("double");
            default:
                return i * 0.5;
        }
    }

    static int wide(int i) {
        int h;
        try {
            long v = wideLong(i);
            h = v == Long.MIN_VALUE ? 1 : (int) v;
        } catch (IllegalStateException e) {
            h = 14000 + e.getMessage().length();
        }
        try {
            double d = wideDouble(i);
            h += (Double.doubleToRawLongBits(d) == Long.MIN_VALUE) ? 7 : (int) (d * 4);
        } catch (IllegalStateException e) {
            h += 20000 + e.getMessage().length();
        }
        return h;
    }

    // ---- 15. an Error caught as Throwable ----
    static int errorCallee(int i) {
        if ((i & 1) == 0) {
            throw new AssertionError("ae" + (i & 3));
        }
        return i;
    }

    static int error(int i) {
        try {
            return errorCallee(i);
        } catch (Throwable t) {
            return 15000 + t.getClass().getSimpleName().length() + t.getMessage().length();
        }
    }

    interface Op {
        int run(int i);
    }

    static long digest(Op op, int n) {
        long h = 17;
        for (int i = 0; i < n; i++) {
            h = h * 31 + op.run(i);
        }
        return h;
    }

    static void conf(StringBuilder out) {
        out.append("conf subclass ").append(digest(Nio21CallThrow::subclass, N)).append('\n');
        out.append("conf second ").append(digest(Nio21CallThrow::second, N)).append('\n');
        finallyCount = 0;
        long fin = digest(Nio21CallThrow::finallyOuter, N);
        out.append("conf finally ").append(fin).append(" count=").append(finallyCount).append('\n');
        out.append("conf nested ").append(digest(Nio21CallThrow::nested, N)).append('\n');
        out.append("conf rethrow ").append(digest(Nio21CallThrow::rethrow, N)).append('\n');
        out.append("conf nomatch ").append(digest(Nio21CallThrow::nomatch, N)).append('\n');
        out.append("conf locals ").append(digest(Nio21CallThrow::locals, N)).append('\n');
        out.append("conf loop ").append(digest(Nio21CallThrow::loop, N)).append('\n');
        out.append("conf reflect ").append(digest(Nio21CallThrow::reflect, N / 4 + 1)).append('\n');
        out.append("conf native ").append(digest(Nio21CallThrow::natives, N / 4 + 1)).append('\n');
        out.append("conf deopt ").append(digest(Nio21CallThrow::deopt, N)).append('\n');
        out.append("conf ctor ").append(digest(Nio21CallThrow::ctor, N)).append('\n');
        out.append("conf iface ").append(digest(Nio21CallThrow::iface, N)).append('\n');
        out.append("conf wide ").append(digest(Nio21CallThrow::wide, N)).append('\n');
        out.append("conf error ").append(digest(Nio21CallThrow::error, N)).append('\n');
    }

    static void row(StringBuilder out, String name, int n, Op op) {
        long h = digest(op, n);
        h ^= digest(op, n);
        long t0 = System.nanoTime();
        h ^= digest(op, n);
        long ns = (System.nanoTime() - t0) / n;
        sink += (int) h;
        out.append(name).append(' ').append(ns).append(" ns/op\n");
    }

    public static void main(String[] args) {
        StringBuilder out = new StringBuilder();
        conf(out);
        System.out.print(out);
        if (ROWS.equals("bench")) {
            StringBuilder b = new StringBuilder();
            row(b, "subclass", N, Nio21CallThrow::subclass);
            row(b, "second", N, Nio21CallThrow::second);
            row(b, "loop", N / 4 + 1, Nio21CallThrow::loop);
            row(b, "reflect", N / 4 + 1, Nio21CallThrow::reflect);
            row(b, "ctor", N, Nio21CallThrow::ctor);
            row(b, "wide", N, Nio21CallThrow::wide);
            System.out.print(b);
        }
    }
}
