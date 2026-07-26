import java.util.*;
import java.util.concurrent.atomic.*;
import java.util.stream.*;

// Verification for keycloak-16 Part A (close handlers). Each line PASS on HotSpot.
public class StreamClose2 {
    static void check(String name, boolean ok) {
        System.out.println((ok ? "PASS " : "FAIL ") + name);
    }
    public static void main(String[] a) {
        // 1. core: onClose + explicit close
        {
            AtomicBoolean c = new AtomicBoolean();
            Stream<Integer> s = Stream.of(1, 2, 3).onClose(() -> c.set(true));
            s.collect(Collectors.toList());
            s.close();
            check("1-core-onClose+close", c.get());
        }
        // 2. propagation through map/filter
        {
            AtomicBoolean c = new AtomicBoolean();
            Stream<Integer> s = Stream.of(1, 2, 3).onClose(() -> c.set(true)).map(x -> x + 1).filter(x -> true);
            s.collect(Collectors.toList());
            s.close();
            check("2-propagate-map/filter", c.get());
        }
        // 3. flatMap closes each mapped inner stream
        {
            AtomicInteger n = new AtomicInteger();
            Stream.of("a", "b").flatMap(v -> Stream.of(1, 2, 3).onClose(n::incrementAndGet)).forEach(x -> {});
            check("3-flatMap-closes-inner (n=" + n.get() + ", want 2)", n.get() == 2);
        }
        // 4. mapToInt propagates upstream handlers; close runs all
        {
            AtomicInteger n = new AtomicInteger();
            IntStream s = Stream.of(1, 2, 3).onClose(n::incrementAndGet).onClose(n::incrementAndGet)
                                 .mapToInt(v -> v).onClose(n::incrementAndGet);
            s.sum();
            s.close();
            check("4-mapToInt-propagate (n=" + n.get() + ", want 3)", n.get() == 3);
        }
        // 5. concat: closing the result closes both inputs
        {
            AtomicInteger n = new AtomicInteger();
            Stream<Integer> s = Stream.concat(
                Stream.of(1, 2).onClose(n::incrementAndGet),
                Stream.of(3, 4).onClose(n::incrementAndGet));
            s.collect(Collectors.toList());
            s.close();
            check("5-concat-closes-both (n=" + n.get() + ", want 2)", n.get() == 2);
        }
        // 6. run-once: double close runs handler once
        {
            AtomicInteger n = new AtomicInteger();
            Stream<Integer> s = Stream.of(1).onClose(n::incrementAndGet);
            s.close();
            s.close();
            check("6-run-once (n=" + n.get() + ", want 1)", n.get() == 1);
        }
    }
}
