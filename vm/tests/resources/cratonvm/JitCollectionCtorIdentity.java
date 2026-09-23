package cratonvm;

import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;

/**
 * JIT-vs-interpreter identity for collections allocated inside a compiled
 * method.
 *
 * <p>Regression fixture for the trivial-constructor elision bug (2026-07-27):
 * {@code is_elidable_construction} judged {@code java/util/HashMap.<init>()V}
 * elidable from its (empty) bytecode body alone, ignoring that a registered
 * native shadows it and allocates the bucket table. With the constructor call
 * elided, a JIT-created {@code HashMap} started with no table at all and the
 * first {@code put} materialised a 32-bucket one (the resize path doubled the
 * assumed default), so the very same keys iterated in a different order than
 * in an interpreter-created map. Iteration order is therefore the observation:
 * it is a direct, cheap readout of the table's capacity.
 *
 * <p>Every observation is printed AFTER a warm-up loop so the allocating
 * methods are JIT-compiled (the harness additionally runs with
 * {@code CRATONVM_JIT_THRESHOLD=1}). The keys are chosen so that a 16-bucket
 * and a 32-bucket table give different orders.
 */
public class JitCollectionCtorIdentity {
    private static final String[] KEYS = {"empty_str", "empty_arr", "empty_obj"};
    private static volatile Object allocationSink;

    private static final class AllocatingValue {
        private final int value;

        private AllocatingValue(int value) {
            this.value = value;
        }

        @Override
        public boolean equals(Object other) {
            // ArrayList.equals is a native loop in CratonVM. Force enough
            // allocation from each element comparison to move the two lists'
            // backing arrays while that loop is in progress.
            byte[][] noise = new byte[4][];
            for (int i = 0; i < noise.length; i++) {
                noise[i] = new byte[64 * 1024];
            }
            allocationSink = noise;
            return other instanceof AllocatingValue
                    && value == ((AllocatingValue) other).value;
        }

        @Override
        public int hashCode() {
            return value;
        }
    }

    private static Map<String, Object> hashMapNoArg() {
        Map<String, Object> m = new HashMap<>();
        for (String k : KEYS) m.put(k, k);
        return m;
    }

    private static Map<String, Object> hashMapSized() {
        Map<String, Object> m = new HashMap<>(16);
        for (String k : KEYS) m.put(k, k);
        return m;
    }

    private static Map<String, Object> linkedHashMapNoArg() {
        Map<String, Object> m = new LinkedHashMap<>();
        for (String k : KEYS) m.put(k, k);
        return m;
    }

    private static Set<String> hashSetNoArg() {
        Set<String> s = new HashSet<>();
        for (String k : KEYS) s.add(k);
        return s;
    }

    private static List<String> arrayListNoArg() {
        List<String> l = new ArrayList<>();
        for (String k : KEYS) l.add(k);
        return l;
    }

    private static Map<String, Object> concurrentHashMapNoArg() {
        Map<String, Object> m = new ConcurrentHashMap<>();
        for (String k : KEYS) m.put(k, k);
        return m;
    }

    private static boolean allocatingArrayListEquals() {
        List<AllocatingValue> a = new ArrayList<>();
        List<AllocatingValue> b = new ArrayList<>();
        List<AllocatingValue> different = new ArrayList<>();
        for (int i = 0; i < 512; i++) {
            a.add(new AllocatingValue(i));
            b.add(new AllocatingValue(i));
            different.add(new AllocatingValue(i == 511 ? -1 : i));
        }
        return a.equals(b) && !a.equals(different);
    }

    private static String render(Iterable<?> it) {
        StringBuilder sb = new StringBuilder();
        for (Object o : it) sb.append(o).append('|');
        return sb.toString();
    }

    public static void main(String[] args) {
        // Warm every allocator past any tier-up threshold before observing.
        for (int i = 0; i < 4000; i++) {
            if (hashMapNoArg().size() != 3
                    || hashMapSized().size() != 3
                    || linkedHashMapNoArg().size() != 3
                    || hashSetNoArg().size() != 3
                    || arrayListNoArg().size() != 3
                    || concurrentHashMapNoArg().size() != 3) {
                throw new IllegalStateException("collection lost entries at iteration " + i);
            }
        }
        System.out.println("r: hashMapNoArg=" + render(hashMapNoArg().keySet()));
        System.out.println("r: hashMapSized=" + render(hashMapSized().keySet()));
        System.out.println("r: linkedHashMap=" + render(linkedHashMapNoArg().keySet()));
        System.out.println("r: hashSet=" + render(hashSetNoArg()));
        System.out.println("r: arrayList=" + render(arrayListNoArg()));
        System.out.println("r: concurrentHashMap="
                + render(concurrentHashMapNoArg().keySet()));
        boolean listEquals = allocatingArrayListEquals();
        if (!listEquals) {
            throw new IllegalStateException(
                    "ArrayList.equals lost backing-array identity across allocation");
        }
        System.out.println("r: allocatingArrayListEquals=true");
        // A map built entry-by-entry across the warm/observe boundary must
        // agree with a freshly built one — this is the json-smart
        // parse/serialize/re-parse round-trip in miniature.
        Map<String, Object> a = hashMapNoArg();
        Map<String, Object> b = new HashMap<>();
        for (Object k : a.keySet()) b.put((String) k, k);
        System.out.println("r: roundTripEqualOrder=" + render(a.keySet()).equals(render(b.keySet())));
        System.out.println("JIT_COLLECTION_CTOR_OK");
    }
}
