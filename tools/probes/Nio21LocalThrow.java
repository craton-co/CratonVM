/**
 * Round nio21 wave 1, lane L2: a throw caught by a handler of the SAME method, in every shape the
 * optimizing tier's compiled local throw edges ({@code CRATONVM_JIT_IR_LOCAL_THROW_EDGES}) decide
 * or decline, so the switch-on arm can be diffed against HotSpot 25 and against the switch-off arm.
 *
 * <p>Each {@code conf} line runs its shape {@code N} times in a loop (so the method is hot and
 * compiled when the line's last iterations run) and prints a digest that depends on which handler
 * ran, the locals the handler saw, the exception's class, message and top stack frame.
 *
 * <pre>
 *   javac -d /tmp/p21 tools/probes/Nio21LocalThrow.java
 *   java     -cp /tmp/p21 Nio21LocalThrow                                   # HotSpot reference
 *   cratonvm -cp /tmp/p21 Nio21LocalThrow                                   # --jdk-only, edges off
 *   CRATONVM_JIT_IR_LOCAL_THROW_EDGES=1 cratonvm -cp /tmp/p21 Nio21LocalThrow
 *   CRATONVM_JIT_IR_LOCAL_THROW_EDGES=1 cratonvm --compatible -cp /tmp/p21 Nio21LocalThrow
 *   # engagement: the `local throw edges` lines name exactLocal, catchAll, loopLocal, ...
 *   CRATONVM_JIT_IR_LOCAL_THROW_EDGES=1 CRATONVM_DBG_IR_COMPILES=1 cratonvm -cp /tmp/p21 \
 *       Nio21LocalThrow 2>&amp;1 | grep 'local throw edges'
 *   # bench rows (ns/op), interleave arms and take medians:
 *   CRATONVM_JIT_IR_LOCAL_THROW_EDGES=1 cratonvm -Dnio21.rows=bench -cp /tmp/p21 Nio21LocalThrow
 * </pre>
 *
 * {@code -Dnio21.n=} sets the loop count per conf line (default 20 000). With {@code
 * -Dnio21.rows=bench} the conf lines are still printed first, then one {@code <row> <N> ns/op} line
 * per row.
 */
public class Nio21LocalThrow {
    static final int N = Integer.getInteger("nio21.n", 20_000);
    static final String ROWS = System.getProperty("nio21.rows", "conf");

    static int sink;

    // 1. The exact class's own catch: the edge.
    static int exactLocal(int i) {
        int a = i * 3;
        try {
            if (i >= 0) {
                a += 7;
                throw new IllegalStateException("exact" + (i & 1));
            }
            return -1;
        } catch (IllegalStateException e) {
            return a + e.getMessage().length() + (e.getStackTrace()[0].getMethodName().equals("exactLocal") ? 100 : 0);
        }
    }

    // 2. A superclass catch: not decided at compile time, the throw leaves as before.
    static int superCatch(int i) {
        try {
            if (i >= 0) {
                throw new IllegalStateException("super");
            }
            return -1;
        } catch (RuntimeException e) {
            return 2 + (e instanceof IllegalStateException ? 10 : 0);
        }
    }

    // 3. Two typed handlers over one range, the second the thrown class.
    static int secondHandler(int i) {
        try {
            if (i >= 0) {
                throw new IllegalArgumentException("second");
            }
            return -1;
        } catch (IllegalStateException e) {
            return 3;
        } catch (IllegalArgumentException e) {
            return 4 + (i & 1);
        }
    }

    // 4. A finally (catch-all) entered by a local throw, then the throw propagates to the caller.
    static int finallyCount;

    static int catchAllInner(int i) {
        try {
            if (i >= 0) {
                throw new UnsupportedOperationException("fin");
            }
            return 0;
        } finally {
            finallyCount++;
        }
    }

    static int catchAll(int i) {
        try {
            return catchAllInner(i);
        } catch (UnsupportedOperationException e) {
            return 5 + e.getMessage().length();
        }
    }

    // 5. Nested try: the inner handler does not match, the outer does.
    static int nested(int i) {
        int x = i & 7;
        try {
            try {
                x += 1;
                if (i >= 0) {
                    throw new ArithmeticException("nested");
                }
            } catch (IllegalStateException e) {
                return -100;
            }
            return -1;
        } catch (ArithmeticException e) {
            return 6 + x;
        }
    }

    // 6. A throw and its catch inside a loop body; the loop continues.
    static int loopLocal(int n) {
        int caught = 0;
        int sum = 0;
        for (int k = 0; k < n; k++) {
            try {
                sum += k;
                if ((k & 3) == 0) {
                    throw new IndexOutOfBoundsException("k" + k);
                }
                sum += 1;
            } catch (IndexOutOfBoundsException e) {
                caught += e.getMessage().length();
            }
        }
        return caught * 31 + sum;
    }

    // 7. The handler throws a new exception, caught by the caller.
    static int rethrowInner(int i) {
        try {
            if (i >= 0) {
                throw new IllegalStateException("inner");
            }
            return 0;
        } catch (IllegalStateException e) {
            throw new IllegalArgumentException("outer:" + e.getMessage());
        }
    }

    static int rethrow(int i) {
        try {
            return rethrowInner(i);
        } catch (IllegalArgumentException e) {
            return e.getMessage().length() + (e.getStackTrace()[0].getMethodName().equals("rethrowInner") ? 1000 : 0);
        }
    }

    // 8. A throw with a value on the operand stack (switch expression).
    static int stackAtThrow(int i) {
        try {
            int v = 10 + switch (i & 1) {
                case 0 -> throw new IllegalStateException("even");
                default -> 1;
            };
            return v;
        } catch (IllegalStateException e) {
            return 8;
        }
    }

    // 9. A throw inside a synchronized block, caught outside it in the same method.
    static final Object LOCK = new Object();

    static int syncLocal(int i) {
        try {
            synchronized (LOCK) {
                if (i >= 0) {
                    throw new IllegalStateException("sync");
                }
            }
            return -1;
        } catch (IllegalStateException e) {
            return 9 + (Thread.holdsLock(LOCK) ? 100 : 0);
        }
    }

    // 10. A rethrow of the caught object itself (not this method's allocation at the athrow).
    static int rethrowSame(int i) {
        try {
            try {
                if (i >= 0) {
                    throw new IllegalStateException("same");
                }
                return -1;
            } catch (IllegalStateException e) {
                throw e;
            }
        } catch (IllegalStateException e) {
            return 10 + e.getMessage().length();
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
        out.append("conf exactLocal ").append(digest(Nio21LocalThrow::exactLocal, N)).append('\n');
        out.append("conf superCatch ").append(digest(Nio21LocalThrow::superCatch, N)).append('\n');
        out.append("conf secondHandler ").append(digest(Nio21LocalThrow::secondHandler, N)).append('\n');
        finallyCount = 0;
        long fin = digest(Nio21LocalThrow::catchAll, N);
        out.append("conf catchAll ").append(fin).append(" finally=").append(finallyCount).append('\n');
        out.append("conf nested ").append(digest(Nio21LocalThrow::nested, N)).append('\n');
        out.append("conf loopLocal ").append(digest(Nio21LocalThrow::loopLocal, 200)).append(' ')
                .append(loopLocal(N)).append('\n');
        out.append("conf rethrow ").append(digest(Nio21LocalThrow::rethrow, N)).append('\n');
        out.append("conf stackAtThrow ").append(digest(Nio21LocalThrow::stackAtThrow, N)).append('\n');
        out.append("conf syncLocal ").append(digest(Nio21LocalThrow::syncLocal, N)).append(" held=")
                .append(Thread.holdsLock(LOCK)).append('\n');
        out.append("conf rethrowSame ").append(digest(Nio21LocalThrow::rethrowSame, N)).append('\n');
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
            row(b, "exactLocal", N, Nio21LocalThrow::exactLocal);
            row(b, "superCatch", N, Nio21LocalThrow::superCatch);
            row(b, "secondHandler", N, Nio21LocalThrow::secondHandler);
            row(b, "loopLocal", N / 50 + 1, i -> loopLocal(64));
            row(b, "stackAtThrow", N, Nio21LocalThrow::stackAtThrow);
            System.out.print(b);
        }
    }
}
