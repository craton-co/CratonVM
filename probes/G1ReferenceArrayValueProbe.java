import java.util.*;
import java.util.stream.*;

/**
 * Regression probe: a REFERENCE array element must round-trip any value.
 *
 * A reference array element is a raw 8-byte pointer, so a primitive cannot be
 * stored in one directly — but natives across the tree use a reference array as
 * a generic value store, and the generational heap has always honoured that by
 * auto-boxing into a one-field wrapper and un-boxing on read.
 *
 * G1 had neither half: its encoder wrote `0` for any non-reference value.
 * ZGC-real had the write half and no read half, so the same store came back as
 * the *wrapper object* rather than as zero. The stream pipeline is the loudest
 * victim of both, because `mapToLong`/`mapToDouble` collect primitives into
 * exactly such an array:
 *
 *   stream.mapToDouble(Double::doubleValue).toArray()
 *     default collector -> [1.0, 2.5, 3.75, 100.0]
 *     -XX:+UseG1GC      -> [0.0, 0.0, 0.0, 0.0]
 *     -XX:+UseZGC       -> [0.0, 0.0, 0.0, 0.0], and `boxed()` yields objects
 *                          that print as `?@2` — the un-named wrapper class
 *                          leaking into Java, which also surfaced as
 *                          `ClassCastException: ? cannot be cast to ...`
 *
 * 4-byte elements (`mapToInt`) survived on both, which is what made this read
 * as a width bug rather than a missing arm.
 *
 *   cratonvm --java-home $JDK25 -XX:+UseG1GC -c . G1ReferenceArrayValueProbe
 *   cratonvm --java-home $JDK25 -XX:+UseZGC  -c . G1ReferenceArrayValueProbe
 *
 * (the ZGC arm needs a launcher built with `--features zgc`; without it
 * `-XX:+UseZGC` falls back to the generational collector and the run is
 * vacuous — check stderr for the "unsupported garbage collector" warning.)
 *
 * Broken build: `PROBE-FAILURES=<n>`. Fixed build: `PROBE-OK`. Deterministic,
 * unchanged by `--nojit`, and unchanged at a heap large enough that no
 * collection runs — this is not a collection-time defect.
 */
public class G1ReferenceArrayValueProbe {

    static int failures = 0;

    static void check(String what, boolean ok, Object got) {
        System.out.println((ok ? "  ok   " : "  FAIL ") + what + " got=" + got);
        if (!ok) {
            failures++;
        }
    }

    public static void main(String[] args) {
        Double[] boxed = { 1.0, 2.5, 3.75, 100.0 };
        double[] wantD = { 1.0, 2.5, 3.75, 100.0 };

        check("stream.mapToDouble.toArray",
                Arrays.equals(Arrays.stream(boxed).mapToDouble(Double::doubleValue).toArray(), wantD),
                Arrays.toString(Arrays.stream(boxed).mapToDouble(Double::doubleValue).toArray()));

        // The exact shape PropertiesMeterFilter.convertServiceLevelObjectives
        // uses, which failed with "serviceLevelObjectiveBoundaries must contain
        // only the values greater than 0. Found 0.0".
        double[] slo = Arrays.stream(boxed)
                .map((candidate) -> candidate)
                .filter(Objects::nonNull)
                .mapToDouble(Double::doubleValue)
                .toArray();
        check("map.filter.mapToDouble.toArray", Arrays.equals(slo, wantD), Arrays.toString(slo));

        long[] wantL = { 1, 2, 3, 4 };
        long[] gotL = Arrays.stream(new Long[] { 1L, 2L, 3L, 4L }).mapToLong(Long::longValue).toArray();
        check("mapToLong.toArray", Arrays.equals(gotL, wantL), Arrays.toString(gotL));

        // 4-byte elements survived the broken encoder; keep them so a
        // regression that breaks only the narrow widths is also caught.
        int[] wantI = { 1, 2, 3, 4 };
        int[] gotI = Arrays.stream(new Integer[] { 1, 2, 3, 4 }).mapToInt(Integer::intValue).toArray();
        check("mapToInt.toArray", Arrays.equals(gotI, wantI), Arrays.toString(gotI));

        // A stream built straight from a primitive array never went through the
        // reference-array store, so it worked on the broken build too. Its job
        // here is to be the control.
        check("DoubleStream.of.toArray",
                Arrays.equals(DoubleStream.of(1.0, 2.5, 3.75, 100.0).toArray(), wantD),
                Arrays.toString(DoubleStream.of(1.0, 2.5, 3.75, 100.0).toArray()));

        // Boxing back and forth must not lose the value either.
        double[] roundTrip = Arrays.stream(boxed)
                .mapToDouble(Double::doubleValue)
                .boxed()
                .mapToDouble(Double::doubleValue)
                .toArray();
        check("mapToDouble.boxed.mapToDouble.toArray", Arrays.equals(roundTrip, wantD),
                Arrays.toString(roundTrip));

        // Reductions read the same elements back, so a lost element shows as a
        // wrong sum rather than a wrong array.
        double sum = Arrays.stream(boxed).mapToDouble(Double::doubleValue).sum();
        check("mapToDouble.sum", sum == 107.25, sum);

        long lsum = Arrays.stream(new Long[] { 1L, 2L, 3L, 4L }).mapToLong(Long::longValue).sum();
        check("mapToLong.sum", lsum == 10L, lsum);

        // References must still round-trip as themselves.
        String[] refs = { "a", "b", "c" };
        List<String> got = Arrays.stream(refs).map(String::toUpperCase).collect(Collectors.toList());
        check("reference elements still round-trip", got.equals(List.of("A", "B", "C")), got);

        // ZGC's half of the defect had a different face from G1's: the store
        // side DID box, so the read handed the pipeline the wrapper object
        // instead of zero. That leaks a class with no name into Java, which is
        // what `?@2` and `ClassCastException: ? cannot be cast to ...` were.
        // Assert on the CLASS, not just the value — a value check alone passes
        // vacuously if some future backend boxes into a real `java.lang.Long`.
        Object[] reboxed = Arrays.stream(new Long[] { 7L, 8L }).mapToLong(Long::longValue).boxed().toArray();
        boolean classesOk = reboxed.length == 2;
        for (Object o : reboxed) {
            classesOk &= (o instanceof Long);
        }
        check("mapToLong.boxed elements are java.lang.Long",
                classesOk && reboxed[0].equals(7L) && reboxed[1].equals(8L),
                Arrays.toString(reboxed) + " classes="
                        + (reboxed.length > 0 ? reboxed[0].getClass().getName() : "<empty>"));

        // The same leak, reached through a cast rather than through toString.
        long viaCast = -1L;
        try {
            Object one = Arrays.stream(new Long[] { 42L }).mapToLong(Long::longValue).boxed().findFirst().orElse(null);
            viaCast = ((Long) one).longValue();
        }
        catch (ClassCastException ex) {
            viaCast = -2L;
        }
        check("mapToLong.boxed element casts to Long", viaCast == 42L, viaCast);

        if (failures == 0) {
            System.out.println("PROBE-OK");
        } else {
            System.out.println("PROBE-FAILURES=" + failures);
        }
    }
}
