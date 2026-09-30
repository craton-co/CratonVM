import java.util.HashMap;
import java.util.Map;

// Checks whether System.identityHashCode() (and therefore any hashCode/equals-
// keyed collection like HashMap/WeakHashMap storing a plain Object such as a
// ClassLoader) stays stable across a moving GC cycle. HotSpot caches the
// identity hash in the object header on first use and never recomputes it.
public class IdentityHashStabilityProbe {
    public static void main(String[] args) {
        Object[] objs = new Object[2000];
        int[] before = new int[objs.length];
        for (int i = 0; i < objs.length; i++) {
            objs[i] = new Object();
            before[i] = System.identityHashCode(objs[i]);
        }
        Map<Object, Integer> map = new HashMap<>();
        for (int i = 0; i < objs.length; i++) {
            map.put(objs[i], i);
        }
        // Force garbage and allocate a lot to trigger at least one, ideally
        // several, moving collections.
        for (int r = 0; r < 20; r++) {
            byte[][] garbage = new byte[10000][];
            for (int i = 0; i < garbage.length; i++) {
                garbage[i] = new byte[256];
            }
            System.gc();
        }
        int mismatches = 0;
        int mapMisses = 0;
        for (int i = 0; i < objs.length; i++) {
            int after = System.identityHashCode(objs[i]);
            if (after != before[i]) {
                mismatches++;
                if (mismatches <= 5) {
                    System.out.println("MISMATCH idx=" + i + " before=" + before[i] + " after=" + after);
                }
            }
            Integer found = map.get(objs[i]);
            if (found == null || found != i) {
                mapMisses++;
                if (mapMisses <= 5) {
                    System.out.println("MAP MISS idx=" + i + " found=" + found);
                }
            }
        }
        System.out.println("total=" + objs.length + " hashMismatches=" + mismatches + " mapMisses=" + mapMisses);
    }
}
