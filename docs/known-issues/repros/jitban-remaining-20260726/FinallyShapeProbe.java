// Narrows FinallyBalanceProbe's defect: which exception-handler shape does a
// JIT-compiled method fail to execute when the protected region throws?
//
//   FINALLY      try { n++; throw } finally { n-- }        -- catch-all, rethrows
//   CATCH        try { n++; throw } catch (Boom) { n-- }   -- typed, swallows
//   CATCHALL     try { n++; throw } catch (Throwable) { n-- }
//   CATCHRETHROW try { n++; throw } catch (Boom) { n--; throw } -- typed, rethrows
//   NOTHROW      same shape, callee never throws            -- control
public class FinallyShapeProbe {

    static int n = 0;
    static long sink = 0;

    static class Boom extends RuntimeException {
        Boom() { super(null, null, false, false); }
    }

    static void work(int i, boolean throwing) {
        sink += i;
        if (throwing && (i % 7) == 3) {
            throw new Boom();
        }
    }

    static void shapeFinally(int i) {
        try {
            n++;
            work(i, true);
        } finally {
            n--;
        }
    }

    static void shapeCatch(int i) {
        try {
            n++;
            work(i, true);
        } catch (Boom e) {
            n--;
            return;
        }
        n--;
    }

    static void shapeCatchAll(int i) {
        try {
            n++;
            work(i, true);
        } catch (Throwable t) {
            n--;
            return;
        }
        n--;
    }

    static void shapeCatchRethrow(int i) {
        try {
            n++;
            work(i, true);
        } catch (Boom e) {
            n--;
            throw e;
        }
        n--;
    }

    static void shapeNoThrow(int i) {
        try {
            n++;
            work(i, false);
        } finally {
            n--;
        }
    }

    interface Shape { void run(int i); }

    static void drive(String name, Shape s, int iterations) {
        n = 0;
        int firstLeak = -1;
        for (int i = 0; i < iterations; i++) {
            try {
                s.run(i);
            } catch (Boom e) {
                // expected for the rethrowing shapes
            }
            if (n != 0 && firstLeak < 0) {
                firstLeak = i;
            }
        }
        System.out.println(String.format("%-14s final=%-8d firstLeak=%-8d %s",
                name, n, firstLeak, (n == 0 ? "OK" : "LEAK")));
    }

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 400000;
        drive("FINALLY", FinallyShapeProbe::shapeFinally, iterations);
        drive("CATCH", FinallyShapeProbe::shapeCatch, iterations);
        drive("CATCHALL", FinallyShapeProbe::shapeCatchAll, iterations);
        drive("CATCHRETHROW", FinallyShapeProbe::shapeCatchRethrow, iterations);
        drive("NOTHROW", FinallyShapeProbe::shapeNoThrow, iterations);
        System.out.println("sink=" + sink);
    }
}
