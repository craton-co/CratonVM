/**
 * Does a JVM local slot that javac reuses for two DIFFERENT type categories
 * (an `int` loop counter, then a `double` after the loop) miscompile?
 *
 * javac freely reuses a slot once the previous variable's scope ends, so
 * `for (int i = ...) {}  double tail = ...;` puts `i` and `tail` in the SAME
 * slot. `find_float_locals` classifies a slot as float if ANY d/f access
 * touches it, so such a slot gets an XMM home for the whole method -- while
 * the loop's `iload`/`istore`/`iinc` on it are integer ops.
 *
 * Four shapes, identical arithmetic, differing only in whether the slot is
 * reused across categories:
 *
 *   reused    int counter, then a double  -- the suspect
 *   noReuse   counter still live after the loop, so it keeps its own slot
 *   intAfter  slot reused, but by another int -- same category
 *   noAfter   nothing after the loop at all
 *
 * All four must print the same `sum`.
 */
public final class SlotReuseCategoryProbe {

    private static int N = 400_000;
    private static double sink;
    private static int intSink;

    private static double reused(double acc0) {
        double sum = 0.0;
        for (int i = 0; i < N; i++) {
            sum += (i % 17) * 0.5 + acc0;
        }
        double tail = sum / 3.0;      // javac reuses `i`'s slot for `tail`
        sink = tail;
        return sum;
    }

    private static double noReuse(double acc0) {
        double sum = 0.0;
        int i = 0;                    // declared outside: still live after
        for (; i < N; i++) {
            sum += (i % 17) * 0.5 + acc0;
        }
        double tail = sum / 3.0;
        sink = tail;
        intSink = i;
        return sum;
    }

    private static double intAfter(double acc0) {
        double sum = 0.0;
        for (int i = 0; i < N; i++) {
            sum += (i % 17) * 0.5 + acc0;
        }
        int tail = (int) (sum / 3.0);  // reuse, but same category
        intSink = tail;
        return sum;
    }

    private static double noAfter(double acc0) {
        double sum = 0.0;
        for (int i = 0; i < N; i++) {
            sum += (i % 17) * 0.5 + acc0;
        }
        return sum;
    }

    /** The reverse order: a double first, then an int loop counter reusing it. */
    private static double doubleThenIntCounter(double acc0) {
        double head = acc0 * 2.0;
        sink = head;
        double sum = 0.0;
        for (int i = 0; i < N; i++) {   // may reuse `head`'s slot
            sum += (i % 17) * 0.5 + acc0;
        }
        return sum;
    }

    public static void main(String[] args) {
        if (args.length > 0) {
            N = Integer.parseInt(args[0]);
        }
        for (int r = 0; r < 3; r++) {
            System.out.println("r" + r
                    + " reused=" + reused(10.0)
                    + " noReuse=" + noReuse(10.0)
                    + " intAfter=" + intAfter(10.0)
                    + " noAfter=" + noAfter(10.0)
                    + " dblThenInt=" + doubleThenIntCounter(10.0));
        }
    }
}
