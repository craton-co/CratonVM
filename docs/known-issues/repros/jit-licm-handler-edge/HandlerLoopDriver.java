/**
 * Drives HandlerLoopProbe.shape(base, n, trip) hot, alternating the normal
 * entry (trip=1, falls through the pre-header) with the exception-handler entry
 * (trip=0 -> div-by-zero, lands inside the loop body without it). Both set max = 25 before the
 * loop, so both must return the same value.
 *
 * n = 8 -> n*5 = 40 -> 25 doubles once to 50, on both paths.
 */
public class HandlerLoopDriver {
    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        final int n = 8;
        final int expected = 50;

        int badNormal = 0;
        int badHandler = 0;
        int firstBad = -1;
        int firstBadValue = 0;
        for (int i = 0; i < iterations; i++) {
            int viaNormal = HandlerLoopProbe.shape(7, n, 1);
            if (viaNormal != expected) {
                badNormal++;
            }
            int viaHandler = HandlerLoopProbe.shape(7, n, 0);
            if (viaHandler != expected) {
                badHandler++;
                if (firstBad < 0) {
                    firstBad = i;
                    firstBadValue = viaHandler;
                }
            }
        }
        System.out.println("DONE iterations=" + iterations
                + " badNormal=" + badNormal
                + " badHandler=" + badHandler
                + (firstBad >= 0 ? " firstBad=" + firstBad + " got=" + firstBadValue : ""));
        if (badNormal > 0 || badHandler > 0) {
            System.exit(1);
        }
    }
}
