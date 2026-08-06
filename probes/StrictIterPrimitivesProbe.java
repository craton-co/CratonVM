import java.util.ArrayList;
import java.util.Arrays;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Spliterator;
import java.util.Spliterators;
import java.util.TreeSet;
import java.util.regex.Pattern;

/**
 * The PRIMITIVES a strict-mode fix for the refused-iterator families could be
 * built out of — measured, not assumed.
 *
 * Two families still die under `--jdk-only` because a native mints a class no
 * JDK declares:
 *
 *   Collections.unmodifiableSet/Map, Map.of, Map.copyOf   -> java/util/HashMap$KeyItr
 *   Collections.unmodifiableSortedSet/NavigableSet        -> java/util/TreeMap$KeyItr
 *   Pattern.splitAsStream                                 -> cratonvm/internal/StreamCollector
 *
 * A fix has to hand back a REAL object instead. Which real construction
 * actually survives strict mode is an empirical question — the interface-level
 * natives this VM registers on `java/util/Iterator` and `java/util/Set` could
 * intercept any of them. Every line below is a candidate building block, so a
 * line that differs from the host JDK rules that block out.
 *
 * Deliberately NOT a success/failure report: each line prints the CONTENT it
 * produced, because a block that returns an empty iteration is the exact
 * failure mode being guarded against and would otherwise read as a pass.
 */
public final class StrictIterPrimitivesProbe {

    public static void main(String[] args) {
        arrayBackedIterators();
        spliteratorAdapters();
        realIterationOfOurCollections();
        System.out.println("StrictIterPrimitivesProbe done");
    }

    /** Candidate building blocks for the HashMap$KeyItr / TreeMap$KeyItr fix. */
    static void arrayBackedIterators() {
        Object[] arr = {"a", "b", "c"};
        p("asList.iterator", () -> drain(Arrays.asList(arr).iterator()));
        p("asList.iterator.class", () -> Arrays.asList(arr).iterator().getClass().getName());
        p("arraylist.iterator", () -> drain(new ArrayList<>(Arrays.asList(arr)).iterator()));
        p("listof.iterator", () -> drain(List.of(arr).iterator()));
        // Does the snapshot list's iterator support remove()? The KeyItr being
        // replaced writes removals through to the backing set.
        p("arraylist.iterator.remove", () -> {
            List<Object> l = new ArrayList<>(Arrays.asList(arr));
            Iterator<Object> it = l.iterator();
            it.next();
            try {
                it.remove();
                return "removed -> " + l;
            } catch (UnsupportedOperationException e) {
                return "UnsupportedOperationException";
            }
        });
        p("asList.iterator.remove", () -> {
            Iterator<Object> it = Arrays.asList(arr).iterator();
            it.next();
            try {
                it.remove();
                return "removed";
            } catch (UnsupportedOperationException e) {
                return "UnsupportedOperationException";
            }
        });
    }

    /** Candidate building block for the StreamCollector fix. */
    static void spliteratorAdapters() {
        p("Arrays.spliterator+adapter", () -> {
            Spliterator<Object> sp = Arrays.spliterator(new Object[] {"x", "y"});
            return drain(Spliterators.iterator(sp));
        });
        p("adapter.class", () ->
                Spliterators.iterator(Arrays.spliterator(new Object[] {"x"})).getClass().getName());
        // The real target: the spliterator behind Pattern.splitAsStream. Taking
        // it through the ADAPTER rather than through a fabricated Consumer is
        // the whole proposal, so this line is the one that decides it.
        p("splitAsStream.spliterator+adapter", () -> {
            Spliterator<String> sp = Pattern.compile(",").splitAsStream("4,5,6").spliterator();
            return drain(Spliterators.iterator(sp));
        });
        p("list.spliterator+adapter", () -> {
            Spliterator<String> sp = List.of("m", "n").spliterator();
            return drain(Spliterators.iterator(sp));
        });
        // tryAdvance driven straight from Java with a real lambda Consumer —
        // proves the spliterator itself yields elements, independent of who
        // the Consumer is.
        p("splitAsStream.tryAdvance", () -> {
            Spliterator<String> sp = Pattern.compile(",").splitAsStream("7,8").spliterator();
            StringBuilder sb = new StringBuilder();
            while (sp.tryAdvance(s -> sb.append(s).append('|'))) {
                // drained by the lambda
            }
            return "[" + sb + "]";
        });
    }

    /**
     * Whether the collections this VM builds natively can be walked by REAL
     * JDK iterator bytecode at all — the claim the deferred fix rests on.
     * `keySet().iterator()` is the native path; `forEach` and the enhanced-for
     * over an entrySet reach the same state by different routes, so a
     * disagreement between these lines localises the gap.
     */
    static void realIterationOfOurCollections() {
        Map<String, String> hm = new java.util.HashMap<>();
        hm.put("h1", "v1");
        hm.put("h2", "v2");
        p("hashmap.forEach", () -> {
            StringBuilder sb = new StringBuilder();
            hm.forEach((k, v) -> sb.append(k).append('=').append(v).append(';'));
            return sb.toString();
        });
        p("hashmap.keySet.size", () -> "" + hm.keySet().size());
        p("hashmap.keySet.iterator", () -> drain(hm.keySet().iterator()));

        Map<String, String> lhm = new LinkedHashMap<>();
        lhm.put("l1", "w1");
        p("linkedhashmap.forEach", () -> {
            StringBuilder sb = new StringBuilder();
            lhm.forEach((k, v) -> sb.append(k).append('=').append(v).append(';'));
            return sb.toString();
        });
        p("linkedhashmap.keySet.iterator", () -> drain(lhm.keySet().iterator()));

        LinkedHashSet<String> lhs = new LinkedHashSet<>(Arrays.asList("s1", "s2"));
        p("linkedhashset.iterator", () -> drain(lhs.iterator()));
        p("linkedhashset.toArray", () -> Arrays.toString(lhs.toArray()));

        TreeSet<String> ts = new TreeSet<>(Arrays.asList("t2", "t1"));
        p("treeset.iterator", () -> drain(ts.iterator()));
        p("treeset.toArray", () -> Arrays.toString(ts.toArray()));
    }

    interface T {
        String get() throws Exception;
    }

    static String drain(Iterator<?> it) {
        StringBuilder sb = new StringBuilder();
        int n = 0;
        while (it.hasNext() && n < 64) {
            if (n > 0) {
                sb.append('|');
            }
            sb.append(it.next());
            n++;
        }
        return "[" + sb + "]/" + n;
    }

    static void p(String label, T t) {
        try {
            System.out.println(label + "=" + t.get());
        } catch (Throwable e) {
            String msg = e.getMessage();
            System.out.println(label + "=" + e.getClass().getName() + (msg == null ? "" : ": " + msg));
        }
    }
}
