// JIT round 18 wave 10, lane throws: a `fillInStackTrace()` refresh must not
// turn suppression back on for a throwable built with `enableSuppression ==
// false` (`docs/feature-designs/jdk-only-mode.md`).
// Deterministic output; every line must equal HotSpot 25's.
public class R18ThrowsSuppression {
    static final class Quiet extends RuntimeException {
        Quiet(String m) { super(m, null, false, true); }
    }

    static final class Loud extends RuntimeException {
        Loud(String m) { super(m); }
    }

    static final class Res implements AutoCloseable {
        public void close() { throw new IllegalStateException("close"); }
    }

    static int refillAndAdd(Throwable t, Throwable s) {
        t.fillInStackTrace();
        t.addSuppressed(s);
        return t.getSuppressed().length;
    }

    static String tryWith(Throwable body) {
        try (Res r = new Res()) {
            throw (RuntimeException) body;
        } catch (RuntimeException e) {
            return e.getClass().getSimpleName() + " suppressed=" + e.getSuppressed().length;
        }
    }

    public static void main(String[] args) {
        RuntimeException extra = new RuntimeException("extra");

        Quiet q = new Quiet("quiet");
        q.addSuppressed(extra);
        System.out.println("quiet-before-refill suppressed=" + q.getSuppressed().length);
        q.fillInStackTrace();
        q.addSuppressed(extra);
        System.out.println("quiet-after-refill suppressed=" + q.getSuppressed().length);

        Loud l = new Loud("loud");
        l.addSuppressed(extra);
        l.fillInStackTrace();
        l.addSuppressed(extra);
        System.out.println("loud-after-refill suppressed=" + l.getSuppressed().length);

        // A refill in a compiled body (hot loop), same answers.
        Quiet hq = new Quiet("hot-quiet");
        int quietMax = 0;
        long loudSum = 0;
        for (int i = 0; i < 20000; i++) {
            quietMax = Math.max(quietMax, refillAndAdd(hq, extra));
            Loud hl = new Loud("hot-loud");
            loudSum += refillAndAdd(hl, extra);
        }
        System.out.println("hot-quiet max-suppressed=" + quietMax);
        System.out.println("hot-loud sum-suppressed=" + loudSum);

        // try-with-resources: the close() failure is dropped for a quiet body
        // (refilled first) and recorded for a loud one.
        Quiet tq = new Quiet("twr-quiet");
        tq.fillInStackTrace();
        System.out.println("twr-quiet " + tryWith(tq));
        Loud tl = new Loud("twr-loud");
        tl.fillInStackTrace();
        System.out.println("twr-loud " + tryWith(tl));
    }
}
