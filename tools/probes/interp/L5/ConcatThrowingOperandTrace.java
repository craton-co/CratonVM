/*
 * Interpreter round i1, wave 20, lane L4: the stack trace of an exception
 * thrown by a string-concat operand's `toString()`, from a method that has
 * been run hot enough to compile.
 *
 * Until wave 20 the compiled concat bridge (`execute_jit_string_concat_raw`)
 * ran the concat on a synthetic interpreter frame, `<jit-indy>.concat`, which
 * sat on the thread's frame stack while the operand's `toString()` ran, so it
 * could appear in the trace of a compiled caller and not in the interpreted
 * one. The trace below keeps every frame outside the JDK (`java.`, `jdk.`,
 * `sun.`, `com.sun.` are dropped, since the JDK's own concat helpers differ by
 * implementation), so a synthetic frame shows up as an extra line.
 *
 * Compare stdout under `--compatible` with and without `--nojit` and against
 * HotSpot 25, which prints:
 *
 *   warm=527534
 *   caught: java.lang.IllegalStateException: bad operand 7
 *     at ConcatThrowingOperandTrace$Bad.toString
 *     at ConcatThrowingOperandTrace.hot
 *     at ConcatThrowingOperandTrace.main
 *   after=x9:1:2
 */
public class ConcatThrowingOperandTrace {
    static final class Bad {
        final int v;

        Bad(int v) {
            this.v = v;
        }

        @Override
        public String toString() {
            if (v == 7) {
                throw new IllegalStateException("bad operand " + v);
            }
            return String.valueOf(v);
        }
    }

    static String hot(Object o, int i, long l) {
        return "x" + o + ":" + i + ":" + l;
    }

    public static void main(String[] args) {
        long sum = 0;
        Bad ok = new Bad(1);
        for (int i = 0; i < 50_000; i++) {
            sum += hot(ok, i & 1023, (i & 1023) * 3L).length();
        }
        System.out.println("warm=" + sum);
        try {
            hot(new Bad(7), 1, 2L);
            System.out.println("no exception");
        } catch (IllegalStateException e) {
            System.out.println("caught: " + e);
            for (StackTraceElement f : e.getStackTrace()) {
                String c = f.getClassName();
                if (c.startsWith("java.") || c.startsWith("jdk.") || c.startsWith("sun.")
                        || c.startsWith("com.sun.")) {
                    continue;
                }
                System.out.println("  at " + c + "." + f.getMethodName());
            }
        }
        // The thread's frame stack is intact after the throw: the next concat
        // from the same compiled method still answers.
        System.out.println("after=" + hot(new Bad(9), 1, 2L));
    }
}
