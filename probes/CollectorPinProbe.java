import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.EnumSet;
import java.util.function.BiConsumer;
import java.util.function.BinaryOperator;
import java.util.function.Function;
import java.util.function.Supplier;
import java.util.stream.Collector;
import java.util.stream.Stream;

/**
 * Positive control for the `Stream.collect(Collector)` GC-safety defect behind
 * `PulsarAutoConfigurationTests`' `java.lang.Object cannot be cast to
 * MultiValueMap`.
 *
 * CratonVM intercepts `Stream.collect(Collector)` with a native. For a
 * collector that is not one of its own tagged fast paths — i.e. any real JDK,
 * Spring or user `Collector`, which is what
 * `MergedAnnotationCollectors.toMultiValueMap` is — it drives the standard
 * protocol by re-entering the interpreter: `supplier()`, `Supplier.get()`,
 * `accumulator()`, `BiConsumer.accept()` per element, `finisher()`,
 * `Function.apply()`. Each of those can allocate, hence collect. If the
 * accumulated container is held across them as a raw reference, a moving young
 * collection relocates it and the caller receives its pre-copy address, whose
 * vacated slot reads back as a bare `java.lang.Object`.
 *
 * This probe makes that window wide instead of rare: the accumulator allocates
 * hard, so a young collection lands INSIDE the accumulate loop nearly every
 * time, and the result is then assigned to the finisher's declared type, which
 * is exactly the checkcast `OnBeanCondition$Spec.<init>` performs.
 *
 * Reports a failure COUNT, so a clean run is a real signal rather than the
 * absence of one.
 */
public class CollectorPinProbe {

    /** A distinct result type, so the assignment below is a real checkcast. */
    public static final class Bag {
        final Map<String, List<String>> m;
        Bag(Map<String, List<String>> m) { this.m = m; }
        int size() { return m.size(); }
    }

    /**
     * Deliberately NOT a `Collectors.*` factory: those are what CratonVM tags
     * and fast-paths. This is the ordinary-Collector shape.
     */
    static final class BagCollector
            implements Collector<String, Map<String, List<String>>, Bag> {
        private final int churn;
        BagCollector(int churn) { this.churn = churn; }

        @Override public Supplier<Map<String, List<String>>> supplier() {
            return LinkedHashMap::new;
        }
        @Override public BiConsumer<Map<String, List<String>>, String> accumulator() {
            return (map, s) -> {
                // Allocation pressure INSIDE the accumulate loop: this is the
                // window in which the container must stay reachable and
                // correctly relocated.
                Object[] junk = new Object[churn];
                for (int i = 0; i < churn; i++) junk[i] = new int[8];
                if (junk[churn - 1] == null) throw new IllegalStateException();
                map.computeIfAbsent(s, k -> new ArrayList<>()).add(s + junk.length);
            };
        }
        @Override public BinaryOperator<Map<String, List<String>>> combiner() {
            return (a, b) -> { a.putAll(b); return a; };
        }
        @Override public Function<Map<String, List<String>>, Bag> finisher() {
            return Bag::new;
        }
        @Override public Set<Characteristics> characteristics() {
            return EnumSet.noneOf(Characteristics.class);
        }
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 300;
        int elems = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        int churn = args.length > 2 ? Integer.parseInt(args[2]) : 400;

        String[] keys = new String[elems];
        for (int i = 0; i < elems; i++) keys[i] = "k" + i;

        int cce = 0, wrongSize = 0, ok = 0, other = 0;
        for (int r = 0; r < rounds; r++) {
            try {
                // The assignment is the checkcast: the native must hand back a
                // Bag, at its CURRENT address.
                Bag bag = Stream.of(keys).collect(new BagCollector(churn));
                if (bag.size() != elems) wrongSize++; else ok++;
            } catch (ClassCastException e) {
                cce++;
                if (cce <= 3) System.out.println("CCE: " + e.getMessage());
            } catch (Throwable t) {
                other++;
                if (other <= 3) System.out.println("OTHER: " + t);
            }
        }
        System.out.println("rounds=" + rounds + " ok=" + ok
                + " classCastException=" + cce
                + " wrongSize=" + wrongSize
                + " other=" + other);
        System.out.println(cce + wrongSize + other == 0 ? "VERDICT=CLEAN" : "VERDICT=BROKEN");
        System.out.println("PROBE_DONE");
    }
}
