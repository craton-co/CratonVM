import java.util.ArrayList;
import java.util.Collections;
import java.util.Iterator;
import java.util.List;

/**
 * `BindConverter.convert` iterates a `private final List` field that holds a
 * `Collections.unmodifiableList(...)` view. Under JIT, the
 * `invokeinterface List.iterator()` there returned null —
 * "Cannot invoke java.util.Iterator.hasNext() because <local5> is null" —
 * failing DevToolsPooledDataSourceAutoConfigurationTests.inMemoryDerbyIsShutdown
 * only once the class had run enough other tests to compile the method.
 * `--nojit` passes.
 *
 * This mirrors the shape: final field, unmodifiable view, hot loop.
 */
public class UnmodifiableListIteratorJitProbe {

    static final class Holder {

        private final List<String> delegates;

        Holder(List<String> source) {
            List<String> copy = new ArrayList<>(source);
            this.delegates = Collections.unmodifiableList(copy);
        }

        /** Shaped like BindConverter.convert: iterate the field, return first hit. */
        String firstMatching(String want) {
            for (String s : this.delegates) {
                if (s.equals(want)) {
                    return s;
                }
            }
            return null;
        }

        /** Explicit iterator() so a null return is observable directly. */
        Iterator<String> rawIterator() {
            return this.delegates.iterator();
        }
    }

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        List<String> source = new ArrayList<>();
        source.add("alpha");
        source.add("beta");
        source.add("gamma");

        int nullIterators = 0;
        int wrongResults = 0;
        long firstNullAt = -1;

        for (int i = 0; i < iterations; i++) {
            Holder h = new Holder(source);
            Iterator<String> it = h.rawIterator();
            if (it == null) {
                nullIterators++;
                if (firstNullAt < 0) {
                    firstNullAt = i;
                }
                continue;
            }
            String hit = h.firstMatching("gamma");
            if (!"gamma".equals(hit)) {
                wrongResults++;
                if (firstNullAt < 0) {
                    firstNullAt = i;
                }
            }
        }

        // Second shape: one long-lived holder, iterated repeatedly (the field is
        // read from a compiled frame many times over).
        Holder shared = new Holder(source);
        int sharedNulls = 0;
        for (int i = 0; i < iterations; i++) {
            if (shared.rawIterator() == null) {
                sharedNulls++;
            }
        }

        System.out.println("iterations       = " + iterations);
        System.out.println("null iterators   = " + nullIterators);
        System.out.println("wrong results    = " + wrongResults);
        System.out.println("shared nulls     = " + sharedNulls);
        System.out.println("first bad at     = " + firstNullAt);
        boolean ok = nullIterators == 0 && wrongResults == 0 && sharedNulls == 0;
        System.out.println(ok ? "PROBE PASS" : "PROBE FAIL");
        System.exit(ok ? 0 : 1);
    }
}
