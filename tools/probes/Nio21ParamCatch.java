/**
 * Round nio21 wave 7, lane L2: single-pass {@code try/catch} methods whose handlers (and the code
 * reachable from them) read only parameters, the population
 * {@code CRATONVM_JIT_LOCAL_HANDLERS_WITHOUT_PRECISE_FRAMES} arms a compiled catch for
 * ({@code r21-single-pass-local-handlers-arm-only-with-precise-frames-CLOSED-20261012.md}). Each shape is
 * one the flip must keep equal to HotSpot: a parameter REASSIGNED inside the {@code try} before the
 * throw (the compiled catch reads its register home, the interpreter path its frame), a throw from
 * a callee and from an {@code athrow}, two handlers over one range, a nested {@code try}, and a
 * throw no local handler takes.
 *
 * <pre>
 *   javac -d /tmp/p21 tools/probes/Nio21ParamCatch.java
 *   java     -cp /tmp/p21 Nio21ParamCatch                                        # HotSpot
 *   CRATONVM_JIT_NO_EXC_TABLE_C2=1 cratonvm -cp /tmp/p21 Nio21ParamCatch            # arm off
 *   CRATONVM_JIT_NO_EXC_TABLE_C2=1 CRATONVM_JIT_LOCAL_HANDLERS_WITHOUT_PRECISE_FRAMES=1 \
 *       cratonvm -cp /tmp/p21 Nio21ParamCatch                                     # arm on
 *   (and both with --compatible)
 *   # bench rows (ns/op):
 *   ... -Dnio21.rows=bench ...
 * </pre>
 *
 * {@code -Dnio21.n=} sets the loop count per conf line (default 20 000).
 */
public class Nio21ParamCatch {
    static final int N = Integer.getInteger("nio21.n", 20_000);
    static final String ROWS = System.getProperty("nio21.rows", "conf");

    static int sink;

    static void thrower(int i) {
        if (i >= 0) {
            throw new IllegalStateException("t" + (i & 3));
        }
        sink++;
    }

    // 1. A parameter reassigned in the try, then a callee throws; the handler reads it.
    static int reassigned(int i) {
        try {
            i = i * 3 + 1;
            thrower(i);
            return -1;
        } catch (IllegalStateException e) {
            return i + e.getMessage().length();
        }
    }

    // 2. An athrow in the method itself, handler reads the parameter only.
    static int local(int i) {
        try {
            if (i >= 0) {
                throw new IllegalArgumentException();
            }
            return -1;
        } catch (IllegalArgumentException e) {
            return 2 + (i & 3);
        }
    }

    // 3. Two handlers over one range; the second takes it.
    static int second(int i) {
        try {
            thrower(i);
            return -1;
        } catch (IllegalArgumentException e) {
            return 3;
        } catch (IllegalStateException e) {
            return 4 + (i & 1);
        }
    }

    // 4. A nested try: the inner handler rethrows a new exception the outer one takes.
    static int nested(int i) {
        try {
            try {
                thrower(i);
                return -1;
            } catch (IllegalStateException e) {
                throw new UnsupportedOperationException("u" + (i & 1));
            }
        } catch (UnsupportedOperationException e) {
            return 5 + e.getMessage().length() + (i & 7);
        }
    }

    // 5. No local handler matches: the throw leaves to the caller.
    static int miss(int i) {
        try {
            thrower(i);
            return -1;
        } catch (IllegalArgumentException e) {
            return 6;
        }
    }

    static int missOuter(int i) {
        try {
            return miss(i);
        } catch (IllegalStateException e) {
            return 7 + (i & 1) + (e.getStackTrace()[0].getMethodName().equals("thrower") ? 10 : 0);
        }
    }

    interface Op {
        int run(int i);
    }

    static long digest(Op op, int n) {
        long d = 0;
        for (int i = 0; i < n; i++) {
            d = d * 31 + op.run(i);
        }
        return d;
    }

    static void row(String name, int n, Op op) {
        for (int rep = 0; rep < 3; rep++) {
            long t0 = System.nanoTime();
            long d = digest(op, n);
            long t1 = System.nanoTime();
            System.out.println(name + " " + n + " " + ((t1 - t0) / n) + " ns/op" + (d == 0 ? "!" : ""));
        }
    }

    public static void main(String[] args) {
        System.out.println("conf reassigned " + digest(Nio21ParamCatch::reassigned, N));
        System.out.println("conf local " + digest(Nio21ParamCatch::local, N));
        System.out.println("conf second " + digest(Nio21ParamCatch::second, N));
        System.out.println("conf nested " + digest(Nio21ParamCatch::nested, N));
        System.out.println("conf missOuter " + digest(Nio21ParamCatch::missOuter, N));
        System.out.println("conf sink " + sink);
        if (!ROWS.equals("bench")) {
            return;
        }
        row("reassigned", N, Nio21ParamCatch::reassigned);
        row("local", N, Nio21ParamCatch::local);
        row("second", N, Nio21ParamCatch::second);
        row("nested", N, Nio21ParamCatch::nested);
        row("missOuter", N, Nio21ParamCatch::missOuter);
    }
}
