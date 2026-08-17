import java.util.*;
import java.util.concurrent.ConcurrentHashMap;

/**
 * Measured population for ConcurrentHashMap iteration order. Builds many maps
 * of varying size from varying key shapes and prints one line per map: the
 * size, and the iteration order as indices into the insertion sequence. Run
 * under HotSpot and under CratonVM and diff — every differing line is a map
 * whose iteration order diverges.
 *
 * Printing INDICES rather than keys keeps the diff readable and makes the
 * failure mode ("which insertion position came out where") legible directly.
 */
public class ChmOrderCensus {
    static String[] gen(int n, int shape, long seed) {
        Random r = new Random(seed);
        String[] ks = new String[n];
        for (int i = 0; i < n; i++) {
            switch (shape) {
                case 0: ks[i] = "K" + r.nextInt(1000000); break;
                case 1: ks[i] = "CONSTRAINT_" + i + (r.nextBoolean() ? "0" : ""); break;
                case 2: ks[i] = Integer.toString(r.nextInt(64)) + "_" + i; break;
                case 3: { // short, low-entropy — maximises collisions
                    char a = (char) ('A' + r.nextInt(4));
                    char b = (char) ('a' + r.nextInt(4));
                    ks[i] = "" + a + b + i;
                    break;
                }
                default: ks[i] = "key-" + Integer.toHexString(r.nextInt()); break;
            }
        }
        // de-duplicate so every insertion is a distinct mapping
        LinkedHashSet<String> uniq = new LinkedHashSet<>(Arrays.asList(ks));
        return uniq.toArray(new String[0]);
    }

    public static void main(String[] args) {
        StringBuilder sb = new StringBuilder();
        int diverged = 0, total = 0;
        for (int shape = 0; shape <= 4; shape++) {
            for (int n : new int[]{1, 2, 3, 5, 8, 11, 12, 13, 16, 20, 24, 25, 32, 40, 48, 49, 64, 100, 200}) {
                for (long seed = 1; seed <= 6; seed++) {
                  for (int ctor = 0; ctor <= 3; ctor++) {
                    String[] keys = gen(n, shape, seed * 7919 + shape * 31 + n);
                    Map<String, Integer> pos = new HashMap<>();
                    for (int i = 0; i < keys.length; i++) pos.put(keys[i], i);

                    // ctor: 0 = default, 1..3 = explicitly sized. The sized
                    // constructors pre-size the JDK's table to
                    // tableSizeFor(n + n/2 + 1), which can exceed what the
                    // entry count alone implies — so an order rule derived
                    // only from size would silently regress them.
                    ConcurrentHashMap<String, Integer> m;
                    switch (ctor) {
                        case 1:  m = new ConcurrentHashMap<>(16); break;
                        case 2:  m = new ConcurrentHashMap<>(Math.max(1, keys.length)); break;
                        case 3:  m = new ConcurrentHashMap<>(512); break;
                        default: m = new ConcurrentHashMap<>(); break;
                    }
                    for (int i = 0; i < keys.length; i++) m.put(keys[i], i);

                    StringBuilder order = new StringBuilder();
                    for (String k : m.keySet()) order.append(pos.get(k)).append(',');
                    // values() and entrySet() must agree with keySet()
                    StringBuilder vorder = new StringBuilder();
                    for (Integer v : m.values()) vorder.append(v).append(',');
                    StringBuilder eorder = new StringBuilder();
                    for (Map.Entry<String, Integer> e : m.entrySet()) eorder.append(e.getValue()).append(',');

                    // The spread hashes in insertion order, so an offline
                    // simulator can predict the order under a candidate rule
                    // without rebuilding the VM.
                    StringBuilder hs = new StringBuilder();
                    for (String k : keys) {
                        int h = k.hashCode();
                        hs.append((h ^ (h >>> 16)) & 0x7fffffff).append(',');
                    }
                    sb.append("shape=").append(shape).append(" n=").append(keys.length)
                      .append(" seed=").append(seed).append(" ctor=").append(ctor)
                      .append(" keys=[").append(order).append(']')
                      .append(" vals=[").append(vorder).append(']')
                      .append(" ents=[").append(eorder).append(']')
                      .append(" sp=[").append(hs).append(']')
                      .append(System.lineSeparator());
                    total++;
                  }
                }
            }
        }
        System.out.print(sb);
        System.out.println("CENSUS_MAPS=" + total);
        System.out.println("CENSUS_END");
    }
}
