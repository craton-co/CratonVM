import java.util.List;
import org.wildfly.common.iteration.CompositeIterable;

/** Regression witness for the iterable path used by VersionedNamespace.createURN. */
public final class WildflyControllerIteratorProbe {
    public static void main(String[] args) {
        for (int round = 0; round < 100_000; round++) {
            Iterable<String> parts = new CompositeIterable<>(
                    List.of("urn"), List.of("controller"), List.of("default"));
            String joined = String.join(":", parts);
            if (!"urn:controller:default".equals(joined)) {
                throw new AssertionError("round=" + round + " joined=" + joined);
            }
            int count = 0;
            for (String part : parts) {
                if (part == null) {
                    throw new AssertionError("round=" + round + " null iterable element");
                }
                count++;
            }
            if (count != 3) {
                throw new AssertionError("round=" + round + " count=" + count);
            }
        }
        System.out.println("WILDFLY_CONTROLLER_ITERATOR_PROBE: PASS");
    }
}