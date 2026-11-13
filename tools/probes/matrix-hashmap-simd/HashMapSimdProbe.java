import java.util.HashMap;
import java.util.Random;

public final class HashMapSimdProbe {
    public static void main(String[] args) {
        long hash = 0;
        for (boolean simd : new boolean[]{false, true}) {
            HashMapSimd.IntMap actual = new HashMapSimd.IntMap(simd);
            HashMap<Integer,Integer> expected = new HashMap<>();
            if (actual.size() != 0 || actual.getOrDefault(0, 57) != 57
                    || actual.getOrDefault(Integer.MIN_VALUE, 57) != 57)
                throw new AssertionError("empty map");
            actual.put(0, 0);
            expected.put(0, 0);
            Random r = new Random(9365);
            for (int i=0; i<12000; i++) {
                int key = switch (i % 5) {
                    case 0 -> 0;
                    case 1 -> Integer.MIN_VALUE;
                    case 2 -> Integer.MAX_VALUE;
                    case 3 -> ((i % 1100) * 65537) ^ 15;
                    default -> r.nextInt();
                };
                int value = r.nextInt();
                actual.put(key, value);
                expected.put(key, value);
                if (actual.size() != expected.size() || actual.getOrDefault(key, 0) != value)
                    throw new AssertionError("insert " + i);
                int absent = r.nextInt();
                if (actual.getOrDefault(absent, 57) != expected.getOrDefault(absent, 57))
                    throw new AssertionError("lookup " + i);
            }
            for (var entry : expected.entrySet())
                if (actual.getOrDefault(entry.getKey(), 0) != entry.getValue()) throw new AssertionError("rehash");
            // Hash the result in deterministic key order, independent of HashMap iteration.
            for (int i=0; i<12000; i++) hash = hash*31+actual.getOrDefault(i << 16, 57);
            if (HashMapSimd.run(1_000_000, simd) != 15499991500000L) throw new AssertionError("benchmark checksum");
        }
        System.out.println("HASHMAP_SIMD_OK " + hash);
    }
}
