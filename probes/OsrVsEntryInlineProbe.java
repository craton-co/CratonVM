import io.netty.handler.codec.http.HttpStatusClass;

import static org.junit.jupiter.api.Assertions.assertEquals;

/**
 * How much does the OSR door lose by planning NO inline sites?
 *
 * `compile_osr_artifact` hands `x64::compile_with_param_slots` an empty
 * `inline_sites` map, so an OSR artifact splices nothing — ever. Every `@Test`
 * body with a hot loop is invoked exactly once, so an OSR artifact is the ONLY
 * compiled form it will ever have, and both netty exhaustive-loop pages are
 * about exactly that shape. The whole inline chain those pages document
 * therefore reaches their loops only INSIDE the callees, never at the call
 * sites in the loop itself.
 *
 * This probe measures the difference directly, on one loop body, in one run:
 *
 *   osrOnce      one call, `n` iterations inside it. OSR is the only door.
 *   entryMany    `n/CHUNK` calls of CHUNK iterations each. The same total work
 *                and the same loop body, but the method crosses the invocation
 *                threshold and gets a METHOD-ENTRY artifact, which is the door
 *                that plans inline sites.
 *
 * The two are not perfectly comparable — `entryMany` pays a call and a
 * loop-setup per chunk, and its inner loop is shorter — so CHUNK is large
 * enough (65536) that both are amortised to well under a nanosecond per
 * iteration. Read the ratio as an upper bound on what wiring the planner into
 * the OSR door could buy, not as the buy itself.
 *
 *   cratonvm --java-home &lt;jdk&gt; @common.args OsrVsEntryInlineProbe 20000000
 *   CRATONVM_JIT_MAIN_INLINE=1 cratonvm ... OsrVsEntryInlineProbe 20000000
 *
 * `CRATONVM_JIT_MAIN_INLINE` is the gate on the method-entry door's planner,
 * so the interesting arm is that one: with it off, both doors plan nothing at
 * the top level and the two arms should agree.
 */
public final class OsrVsEntryInlineProbe {

    private static final int CHUNK = 65536;

    static int sink;

    /** The `testHttpStatusClassValueOf` body, one exhaustive rung. */
    private static void body(int from, int count) {
        for (int k = 0; k < count; k++) {
            int code = from + k;
            HttpStatusClass c = HttpStatusClass.valueOf(code);
            assertEquals(HttpStatusClass.UNKNOWN, c);
            sink += c.ordinal();
        }
    }

    /** Reached ONCE: OSR is the only route out of the interpreter. */
    static void osrOnce(int n) {
        for (int k = 0; k < n; k++) {
            int code = Integer.MIN_VALUE + k;
            HttpStatusClass c = HttpStatusClass.valueOf(code);
            assertEquals(HttpStatusClass.UNKNOWN, c);
            sink += c.ordinal();
        }
    }

    /** Reached many times: the method-entry door compiles it. */
    static void entryMany(int n) {
        int chunks = Math.max(1, n / CHUNK);
        for (int c = 0; c < chunks; c++) {
            body(Integer.MIN_VALUE + c * CHUNK, CHUNK);
        }
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20_000_000;
        int chunks = Math.max(1, n / CHUNK);
        long iters = (long) chunks * CHUNK;

        // Warm `body` past the invocation threshold before timing it, so the
        // `entryMany` arm measures the compiled form rather than the ramp.
        for (int c = 0; c < 2000; c++) {
            body(Integer.MIN_VALUE, 8);
        }

        long t0 = System.nanoTime();
        osrOnce((int) iters);
        long t1 = System.nanoTime();
        entryMany(n);
        long t2 = System.nanoTime();

        System.out.printf("osrOnce    %8.2f ns/iter%n", (double) (t1 - t0) / iters);
        System.out.printf("entryMany  %8.2f ns/iter%n", (double) (t2 - t1) / iters);
        System.out.println("sink=" + sink);
    }
}
