import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashSet;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Set;

/**
 * `new HashSet<>(collection)` correctness under allocation pressure.
 *
 * The HashSet(Collection) native allocates its backing map BEFORE it reads the
 * source collection's elements. If the source argument is not rooted across
 * that allocation, a moving young GC there leaves it pointing into from-space
 * and the constructed set gets the wrong elements (or the element hash/equals
 * dispatch lands on a relocated object). Run with
 *   CRATONVM_MOVING_YOUNG=1 CRATONVM_DBG_GC_STRESS=<bytes>
 * to force a collection inside that window on essentially every construction.
 */
public class HashSetInitProbe {

    static final class Key {
        final String name;

        Key(String name) {
            this.name = name;
        }

        @Override
        public int hashCode() {
            return name.hashCode();
        }

        @Override
        public boolean equals(Object o) {
            return o instanceof Key && name.equals(((Key) o).name);
        }
    }

    static volatile Object sink;

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 28;
        long failures = 0;

        for (int r = 0; r < rounds; r++) {
            Key[] keys = new Key[n];
            for (int i = 0; i < n; i++) {
                keys[i] = new Key(new String(("attr-" + r + "-" + i).toCharArray()));
            }
            List<Key> list = Arrays.asList(keys);

            Set<Key> hs = new HashSet<>(list);
            Set<Key> lhs = new LinkedHashSet<>(list);

            String bad = check("HashSet", hs, keys);
            if (bad == null) {
                bad = check("LinkedHashSet", lhs, keys);
            }
            if (bad == null) {
                // Every key must be removable — this is what WildFly's
                // TransactionSubsystemRootResourceDefinition.registerAttributes
                // depends on, and a silently-failed remove registers twice.
                Set<Key> copy = new HashSet<>(list);
                for (Key k : keys) {
                    copy.remove(k);
                }
                if (!copy.isEmpty()) {
                    List<String> left = new ArrayList<>();
                    for (Key k : copy) {
                        left.add(k.name);
                    }
                    bad = "remove-left-behind " + left;
                }
            }
            if (bad != null) {
                failures++;
                System.out.println("FAIL round=" + r + " " + bad);
                System.out.flush();
                if (failures > 20) {
                    break;
                }
            }
            sink = new byte[512];
        }
        System.out.println("DONE rounds=" + rounds + " failures=" + failures);
        System.out.println(failures == 0 ? "PASS" : "PROBE-FAILED");
    }

    private static String check(String what, Set<Key> set, Key[] keys) {
        if (set.size() != keys.length) {
            return what + " size=" + set.size() + " expected=" + keys.length;
        }
        for (Key k : keys) {
            boolean seen = false;
            for (Key e : set) {
                if (e.name.equals(k.name)) {
                    seen = true;
                    break;
                }
            }
            if (!seen) {
                return what + " missing=" + k.name;
            }
        }
        return null;
    }
}
