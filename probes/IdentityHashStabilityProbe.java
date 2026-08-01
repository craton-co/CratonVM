/**
 * Is `System.identityHashCode(o)` stable across a GC move for an object
 * allocated by the JIT's inline `new` fast path?
 *
 * CratonVM keys every per-object native side table on the identity hash
 * (`native-collections::widened_obj_key`), so an unstable hash silently hands a
 * live collection a FRESH, EMPTY overlay: the map reads back as `size 0` with
 * no dangling pointer and nothing for any heap verifier to find.
 *
 * The JIT's `new` fast path leaves the header hash at 0 and relies on a
 * lazy-mint-and-CAS on first read; this probe checks that contract holds after
 * the allocation site has tiered up (hence the warmup loop) and across several
 * collections.
 *
 *   cratonvm IdentityHashStabilityProbe
 */
import java.util.ArrayList;
import java.util.List;
import java.util.TreeMap;

public class IdentityHashStabilityProbe {
    static final class Holder {
        final int id;
        Holder(int id) { this.id = id; }
    }

    public static void main(String[] args) {
        final int warmup = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        final int tracked = args.length > 1 ? Integer.parseInt(args[1]) : 64;

        // Tier up the `new` sites before recording anything: an interpreted
        // allocation mints eagerly, so a probe that never warms up cannot
        // express the bug.
        long sink = 0;
        for (int i = 0; i < warmup; i++) {
            sink += new Holder(i).id;
            sink += new TreeMap<String, String>().size();
        }

        List<Holder> holders = new ArrayList<>();
        List<TreeMap<String, String>> maps = new ArrayList<>();
        int[] holderHash = new int[tracked];
        int[] mapHash = new int[tracked];
        for (int i = 0; i < tracked; i++) {
            Holder h = new Holder(i);
            TreeMap<String, String> m = new TreeMap<>();
            m.put("k" + i, "v" + i);
            holders.add(h);
            maps.add(m);
            holderHash[i] = System.identityHashCode(h);
            mapHash[i] = System.identityHashCode(m);
        }

        int holderDrift = 0;
        int mapDrift = 0;
        int contentLoss = 0;
        for (int round = 0; round < 8; round++) {
            for (int i = 0; i < 40000; i++) {
                sink += new StringBuilder("x").append(i).toString().length();
            }
            System.gc();
            for (int i = 0; i < tracked; i++) {
                if (System.identityHashCode(holders.get(i)) != holderHash[i]) holderDrift++;
                if (System.identityHashCode(maps.get(i)) != mapHash[i]) mapDrift++;
                TreeMap<String, String> m = maps.get(i);
                if (m.size() != 1 || !("v" + i).equals(m.get("k" + i))) contentLoss++;
            }
        }

        System.out.println("CK tracked=" + tracked + " holderDrift=" + holderDrift
                + " mapDrift=" + mapDrift + " contentLoss=" + contentLoss
                + " sink=" + (sink != 0));
        if (holderDrift != 0 || mapDrift != 0) {
            throw new AssertionError("identity hash moved with the object: holder="
                    + holderDrift + " map=" + mapDrift);
        }
        if (contentLoss != 0) {
            throw new AssertionError("TreeMap lost its contents " + contentLoss + " time(s)");
        }
        System.out.println("PASS IdentityHashStabilityProbe");
    }
}
