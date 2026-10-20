/**
 * The aarch64 backend's h23 lane, end to end: a hot method whose body is a
 * `synchronized` block containing a `try`/`catch`, a reference `getstatic`
 * and a reference `getfield`.
 *
 * Every one of those was a refusal on that backend until 2026-09-22:
 *
 *  * the `synchronized` block emits `monitorenter`/`monitorexit`, which had
 *    no arm at all, and carries javac's generated `any -> monitorexit; athrow`
 *    handler, which made the method's exception table non-empty -- itself a
 *    blanket refusal for every trapping lowering;
 *  * the explicit `try`/`catch` is a second exception-table entry, so the
 *    handler search has two candidates and the throw-site bci has to be right
 *    for the drain to pick between them;
 *  * `SHARED` is a reference static (`getstatic` of type 'L');
 *  * `Cell.next` is a reference field (`getfield` of type 'L').
 *
 * The answer is checked against a real JDK rather than against itself: run
 * this under HotSpot and under `cratonvm` with `CRATONVM_JIT_ARM64=1` and a
 * low tier-up threshold, and the two lines must be identical.
 */
public final class A64SyncProbe {

    /** A reference static: `getstatic` of type 'L'. */
    static final Object SHARED = new Object();

    /** Gives `work` a reference field to read. */
    static final class Cell {
        final int value;
        final Cell next;

        Cell(int value, Cell next) {
            this.value = value;
            this.next = next;
        }
    }

    private static Cell chain(int n) {
        Cell c = null;
        for (int i = 0; i < n; i++) {
            c = new Cell(i, c);
        }
        return c;
    }

    /**
     * The hot method. One `synchronized` block, one `try`/`catch` inside it,
     * a reference static read and a reference field walk.
     *
     * The two handlers are deliberately nested so that a throw-site bci that
     * named the wrong site would route into the monitor's `any` handler
     * instead of the `catch`, which shows up as a different total rather
     * than as a crash.
     */
    static int work(Cell head, int n) {
        int total = 0;
        synchronized (SHARED) {
            Cell c = head;
            for (int i = 0; i < n; i++) {
                try {
                    if (i % 7 == 0) {
                        throw new IllegalStateException("seven");
                    }
                    total += i;
                } catch (IllegalStateException e) {
                    total -= 1;
                }
                // A reference `getfield`, and a null one every lap of the
                // chain so the null check is really exercised.
                if (c == null) {
                    c = head;
                } else {
                    total += c.value;
                    c = c.next;
                }
            }
        }
        return total;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2000;
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 200;
        Cell head = chain(64);
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            sum += work(head, n);
        }
        System.out.println("A64SyncProbe sum=" + sum);
    }
}
