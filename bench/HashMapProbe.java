// NEW-1.3 / SPB.1 retest probe: hammer java.util.HashMap's banned methods
// (put -> putVal/newNode/hash, get, remove, resize via small initial capacity,
// treeify via colliding keys) hard enough to cross every JIT threshold, then
// print order-independent checksums that must match HotSpot exactly.
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.Map;

public final class HashMapProbe {
    // Key whose hashCode collides in buckets to force long chains + treeify.
    static final class Collider {
        final int id;
        Collider(int id) { this.id = id; }
        @Override public int hashCode() { return id & 0xF; }
        @Override public boolean equals(Object o) {
            return o instanceof Collider && ((Collider) o).id == id;
        }
    }

    public static void main(String[] args) {
        int outer = args.length > 0 ? Integer.parseInt(args[0]) : 300;

        long sumInt = 0;
        long sumStr = 0;
        long sumCol = 0;
        long sumLhm = 0;
        long sizes = 0;

        for (int r = 0; r < outer; r++) {
            // Integer keys, tiny initial capacity -> many resizes.
            HashMap<Integer, Integer> mi = new HashMap<>(2);
            for (int i = 0; i < 2000; i++) {
                mi.put(i, i * 31 + r);
            }
            for (int i = 0; i < 2000; i += 3) {
                mi.remove(i);
            }
            for (int i = 0; i < 2000; i++) {
                Integer v = mi.get(i);
                if (v != null) {
                    sumInt += v.intValue();
                }
            }
            sizes += mi.size();

            // String keys.
            HashMap<String, Integer> ms = new HashMap<>(2);
            for (int i = 0; i < 1500; i++) {
                ms.put("key" + i, i ^ r);
            }
            for (int i = 0; i < 1500; i++) {
                Integer v = ms.get("key" + i);
                sumStr += (v == null) ? -1 : v.intValue();
            }
            sizes += ms.size();

            // Colliding keys -> chains + treeify + treeified removal.
            HashMap<Collider, Integer> mc = new HashMap<>(2);
            for (int i = 0; i < 600; i++) {
                mc.put(new Collider(i), i + r);
            }
            for (int i = 0; i < 600; i += 2) {
                mc.remove(new Collider(i));
            }
            for (int i = 0; i < 600; i++) {
                Integer v = mc.get(new Collider(i));
                sumCol += (v == null) ? -7 : v.intValue();
            }
            sizes += mc.size();

            // LinkedHashMap (banned newNode/afterNode* family) incl. iteration order.
            LinkedHashMap<String, Integer> ml = new LinkedHashMap<>(2);
            for (int i = 0; i < 800; i++) {
                ml.put("L" + i, i);
            }
            int pos = 0;
            for (Map.Entry<String, Integer> e : ml.entrySet()) {
                sumLhm += (long) pos * e.getValue();
                pos++;
            }
            sizes += ml.size();
        }

        System.out.println("sumInt=" + sumInt + " sumStr=" + sumStr + " sumCol=" + sumCol
                + " sumLhm=" + sumLhm + " sizes=" + sizes);
    }
}
