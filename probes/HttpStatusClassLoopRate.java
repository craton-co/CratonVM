import io.netty.handler.codec.http.HttpStatusClass;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotSame;
import static org.junit.jupiter.api.Assertions.fail;

/**
 * HttpResponseStatusTest.testHttpStatusClassValueOf, verbatim, except that the
 * two exhaustive loops are bounded by `n` instead of running the full int range.
 * Called exactly ONCE, like a @Test method, so OSR is the only route out of the
 * interpreter — the shape the real class has.
 */
public final class HttpStatusClassLoopRate {

    static void testHttpStatusClassValueOf(int n) {
        // status scope: [100, 600).
        for (int code = 100; code < 600; code ++) {
            HttpStatusClass httpStatusClass = HttpStatusClass.valueOf(code);
            assertNotSame(HttpStatusClass.UNKNOWN, httpStatusClass);
            if (HttpStatusClass.INFORMATIONAL.contains(code)) {
                assertEquals(HttpStatusClass.INFORMATIONAL, httpStatusClass);
            } else if (HttpStatusClass.SUCCESS.contains(code)) {
                assertEquals(HttpStatusClass.SUCCESS, httpStatusClass);
            } else if (HttpStatusClass.REDIRECTION.contains(code)) {
                assertEquals(HttpStatusClass.REDIRECTION, httpStatusClass);
            } else if (HttpStatusClass.CLIENT_ERROR.contains(code)) {
                assertEquals(HttpStatusClass.CLIENT_ERROR, httpStatusClass);
            } else if (HttpStatusClass.SERVER_ERROR.contains(code)) {
                assertEquals(HttpStatusClass.SERVER_ERROR, httpStatusClass);
            } else {
                fail("At least one of the if-branches above must be true");
            }
        }
        // status scope: [Integer.MIN_VALUE, 100), bounded.
        for (int code = Integer.MIN_VALUE; code < Integer.MIN_VALUE + n; code ++) {
            HttpStatusClass httpStatusClass = HttpStatusClass.valueOf(code);
            assertEquals(HttpStatusClass.UNKNOWN, httpStatusClass);
        }
        // status scope: [600, Integer.MAX_VALUE], bounded.
        for (int code = 600; code < 600 + n; code ++) {
            HttpStatusClass httpStatusClass = HttpStatusClass.valueOf(code);
            assertEquals(HttpStatusClass.UNKNOWN, httpStatusClass);
        }
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20_000_000;
        long t0 = System.nanoTime();
        testHttpStatusClassValueOf(n);
        long t1 = System.nanoTime();
        long iters = 2L * n + 500;
        System.out.printf("real-loop %8.2f ns/iter (%d iters, %d ms) => full 4294967296 iters = %.1f s%n",
                (double) (t1 - t0) / iters, iters, (t1 - t0) / 1_000_000L,
                (double) (t1 - t0) / iters * 4294967296.0 / 1e9);
    }
}
