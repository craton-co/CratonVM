/**
 * gcf f1/r (2026-10-01): the uncaught-exception report's frame order, class
 * name form and cause elision against HotSpot
 * (gce-e2f-uncaught-exception-report-lists-frames-outermost-first-20260929).
 *
 * <p>No stdout: the report is on stderr. Compare the {@code Exception in
 * thread}, {@code Caused by:}, {@code \tat} and {@code \t...} lines of
 * stderr, with the launcher's {@code [cratonvm] main-vm run() returned Err: }
 * prefix stripped. Argument {@code cause} wraps the throw in a
 * RuntimeException (a {@code Caused by:} with {@code ... 1 more}).
 */
public class GcfUncaughtTraceOrderProbe {
    static void inner() {
        throw new IllegalStateException("x");
    }

    static void outer(boolean wrap) {
        if (!wrap) {
            inner();
            return;
        }
        try {
            inner();
        } catch (IllegalStateException e) {
            throw new RuntimeException("wrapped", e);
        }
    }

    public static void main(String[] args) {
        outer(args.length > 0 && args[0].equals("cause"));
    }
}
