import java.util.HashMap;
import java.util.Map;

/**
 * HashMapIterationOrderProbe — differential probe for the Integer-keyed dense
 * overlay's reproduction of real `HashMap` bucket-iteration order.
 *
 * The overlay stores entries in an ascending-key dense vector, not in buckets,
 * so `keys_in_java_hashmap_order` has to *reconstruct* the JDK's order from
 * `(bucket, first-insertion sequence)`. The hashmap-half-gap-20260730 change
 * stopped storing that sequence in a side hash map and now derives it from the
 * key, keeping a `dense_seq_overrides` table only for the keys whose sequence
 * is NOT their key: out-of-order inserts, entries that migrate from the sparse
 * arm into the dense vector, and remove-then-reinsert.
 *
 * This probe prints the iteration order of exactly those shapes. Its output is
 * meant to be diffed against a real JDK run — any divergence is a defect.
 * Nothing here is timing-dependent or hash-seed-dependent: `Integer.hashCode`
 * is the value itself and `HashMap`'s spread is fixed, so a conforming JVM is
 * deterministic.
 */
public class HashMapIterationOrderProbe {

    static void dump(String tag, Map<Integer, Integer> m) {
        StringBuilder sb = new StringBuilder(tag).append(" size=").append(m.size()).append(" keys=[");
        boolean first = true;
        for (Integer k : m.keySet()) {
            if (!first) {
                sb.append(',');
            }
            sb.append(k);
            first = false;
        }
        sb.append("] values=[");
        first = true;
        for (Integer v : m.values()) {
            if (!first) {
                sb.append(',');
            }
            sb.append(v);
            first = false;
        }
        sb.append("] entries=[");
        first = true;
        for (Map.Entry<Integer, Integer> e : m.entrySet()) {
            if (!first) {
                sb.append(',');
            }
            sb.append(e.getKey()).append('=').append(e.getValue());
            first = false;
        }
        sb.append(']');
        System.out.println(sb);
    }

    public static void main(String[] args) {
        // 1. Plain ascending inserts — sequence equals the key, no overrides.
        HashMap<Integer, Integer> ascending = new HashMap<>();
        for (int i = 0; i < 40; i++) {
            ascending.put(i, i * 31 + 7);
        }
        dump("ascending", ascending);

        // 2. Descending inserts — every sequence differs from its key.
        HashMap<Integer, Integer> descending = new HashMap<>();
        for (int i = 39; i >= 0; i--) {
            descending.put(i, i * 31 + 7);
        }
        dump("descending", descending);

        // 3. A key far ahead of the dense frontier starts in the sparse arm,
        //    then migrates into the vector once the frontier passes it. Its
        //    ORIGINAL insertion sequence must survive the migration.
        HashMap<Integer, Integer> migrating = new HashMap<>();
        migrating.put(2048, -1);
        for (int i = 0; i <= 2048; i++) {
            migrating.put(i, i);
        }
        System.out.println("migrating size=" + migrating.size()
                + " head=" + firstKeys(migrating, 8)
                + " v2048=" + migrating.get(2048));

        // 4. Value update must NOT move an entry within its bucket chain.
        HashMap<Integer, Integer> updated = new HashMap<>();
        for (int i = 0; i < 24; i++) {
            updated.put(i, i);
        }
        updated.put(3, 300);
        updated.put(17, 1700);
        dump("updated", updated);

        // 5. Remove then reinsert MUST take a new (later) chain position.
        HashMap<Integer, Integer> reinserted = new HashMap<>();
        for (int i = 0; i < 24; i++) {
            reinserted.put(i, i);
        }
        reinserted.remove(3);
        reinserted.remove(17);
        reinserted.put(3, -3);
        reinserted.put(17, -17);
        dump("reinserted", reinserted);

        // 6. Interleaved: forces overrides for some keys and derived sequences
        //    for others in one map.
        HashMap<Integer, Integer> interleaved = new HashMap<>();
        for (int i = 0; i < 32; i += 2) {
            interleaved.put(i, i);
        }
        for (int i = 1; i < 32; i += 2) {
            interleaved.put(i, i);
        }
        dump("interleaved", interleaved);

        // 7. Negative keys never live in the dense vector.
        HashMap<Integer, Integer> negatives = new HashMap<>();
        for (int i = -8; i < 8; i++) {
            negatives.put(i, i);
        }
        dump("negatives", negatives);

        // 8. Resize past the default threshold with a gap-heavy key set.
        HashMap<Integer, Integer> sparse = new HashMap<>();
        for (int i = 0; i < 40; i++) {
            sparse.put(i * 7, i);
        }
        dump("sparse", sparse);
    }

    static String firstKeys(Map<Integer, Integer> m, int n) {
        StringBuilder sb = new StringBuilder("[");
        int i = 0;
        for (Integer k : m.keySet()) {
            if (i++ == n) {
                break;
            }
            if (i > 1) {
                sb.append(',');
            }
            sb.append(k);
        }
        return sb.append(']').toString();
    }
}
