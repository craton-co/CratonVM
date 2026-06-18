import java.util.*;
import java.util.stream.*;

// Repro for keycloak-16-stream-onclose-and-laziness.
// Part A: Stream.onClose(Runnable) must register the handler and close() must run it.
//         CratonVM's synthetic stream returns `this` and drops the handler; close() is a no-op.
// Part B: intermediate ops must be LAZY (short-circuit). CratonVM eager-materializes, so
//         `peek` sees every element even when the terminal op only needs the first.
public class StreamOnClose {
    public static void main(String[] a) {
        // Part A — onClose / close
        boolean[] closed = { false };
        Stream<String> s = Stream.of("x", "y", "z").onClose(() -> closed[0] = true);
        s.collect(Collectors.toList());
        s.close();
        System.out.println("A: onClose ran on close() = " + closed[0] + "   (HotSpot: true)");

        // Part B — laziness / short-circuit
        List<Integer> peeked = new ArrayList<>();
        Optional<Integer> first = Stream.of(1, 2, 3, 4, 5).peek(peeked::add).findFirst();
        System.out.println("B: findFirst=" + first.orElse(-1) + " peeked=" + peeked.size()
                + "   (HotSpot: peeked=1, lazy short-circuit)");

        boolean ok = closed[0] && peeked.size() == 1;
        System.out.println("RESULT=" + (ok ? "OK" : "FAIL  (A=" + closed[0] + " B-peeked=" + peeked.size() + ")"));
    }
}
