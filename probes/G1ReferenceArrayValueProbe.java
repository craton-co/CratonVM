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
 * G1 had neither half: its encoder wrote `0` for any non-reference value. The
 * stream pipeline is the loudest victim, because `mapToLong`/`mapToDouble`
 * collect primitives into exactly such an array:
 *
 *   stream.mapToDouble(Double::doubleValue).toArray()
 *     default collector -> [1.0, 2.5, 3.75, 100.0]
 *     -XX:+UseG1GC      -> [0.0, 0.0, 0.0, 0.0]
 *
 * 4-byte elements (`mapToInt`) survived, which is what made this read as a
 * width bug rather than a missing arm.
 *
 *   cratonvm --java-home $JDK25 -XX:+UseG1GC -c . G1ReferenceArrayValueProbe
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

        if (failures == 0) {
            System.out.println("PROBE-OK");
        } else {
            System.out.println("PROBE-FAILURES=" + failures);
        }
    }
}
