import java.util.HashMap;

/**
 * Correctness witness for the JIT's exact-HashMap and Integer thin helpers.
 *
 * The hot loop is deliberately declared against HashMap so an exact receiver
 * takes the direct overlay path while a subclass at the same call site must
 * fall back to ordinary virtual dispatch.
 */
public final class HashMapDirectHelpersProbe {
    private static long integerLoop(HashMap<Integer, Integer> map, int n) {
        for (int i = 0; i < n; i++) {
            map.put(i, i * 31 + 7);
        }
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += map.get(i);
        }
        return sum;
    }

    private static final class TrackingMap extends HashMap<Integer, Integer> {
        int puts;
        int gets;

        @Override
        public Integer put(Integer key, Integer value) {
            puts++;
            return super.put(key, value);
        }

        @Override
        public Integer get(Object key) {
            gets++;
            return super.get(key);
        }
    }

    private static long expected(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += i * 31 + 7;
        }
        return sum;
    }

    public static void main(String[] args) {
        int n = args.length == 0 ? 200_000 : Integer.parseInt(args[0]);
        long expected = expected(n);

        HashMap<Integer, Integer> exact = new HashMap<>();
        long exactSum = integerLoop(exact, n);
        if (exactSum != expected || exact.size() != n) {
            throw new AssertionError("exact HashMap mismatch: " + exactSum + "/" + exact.size());
        }

        TrackingMap subclass = new TrackingMap();
        long subclassSum = integerLoop(subclass, n);
        if (subclassSum != expected || subclass.puts != n || subclass.gets != n) {
            throw new AssertionError(
                    "subclass dispatch mismatch: "
                            + subclassSum + "/" + subclass.puts + "/" + subclass.gets);
        }

        HashMap<String, Integer> strings = new HashMap<>();
        strings.put("alpha", 11);
        strings.put("beta", 22);
        if (strings.get("alpha") != 11 || strings.get("beta") != 22) {
            throw new AssertionError("non-Integer fallback mismatch");
        }

        HashMap<Object, String> wrappers = new HashMap<>();
        wrappers.put(Integer.valueOf(65), "integer");
        wrappers.put(Character.valueOf('A'), "character");
        if (wrappers.size() != 2
                || !"integer".equals(wrappers.get(Integer.valueOf(65)))
                || !"character".equals(wrappers.get(Character.valueOf('A')))) {
            throw new AssertionError("cross-wrapper key alias");
        }

        HashMap<Object, Object> nulls = new HashMap<>();
        nulls.put(null, null);
        if (!nulls.containsKey(null) || nulls.get(null) != null) {
            throw new AssertionError("null-key/value mismatch");
        }

        System.out.println(
                "HASHMAP_DIRECT_HELPERS_PASS n=" + n
                        + " checksum=" + exactSum
                        + " subclassCalls=" + subclass.puts + "/" + subclass.gets);
    }
}
