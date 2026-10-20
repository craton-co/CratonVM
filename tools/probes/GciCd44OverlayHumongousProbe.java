// gci i44/cd: humongous arrays held ONLY by long-lived TreeMap / LinkedHashMap /
// LinkedList values, which are then never written again while young
// collections run.
//
// Under G1 (1 MiB regions at -Xmx128m) every value is a humongous object. In
// `--compatible` mode these collections are collection overlays: their
// contents live in Rust side tables, which are not heap slots, so nothing but
// the overlay row names the arrays. Before the i44/cd fix a young pause read
// such a row only while its owner or a value was young; once the collection
// was promoted the row was skipped, and the eager humongous reclaim freed the
// arrays under it (the regions are then reused by Eden: the contents read back
// wrong, or the VM crashes).
//
//   GciCd44OverlayHumongousProbe <values> <bytes> <rounds> <churnMb>
//
// stdout (deterministic): one `cd44` line; `bad=0` on a sound VM.
// stderr: wall time.
import java.util.LinkedHashMap;
import java.util.LinkedList;
import java.util.Map;
import java.util.TreeMap;

public final class GciCd44OverlayHumongousProbe {
    static TreeMap<Integer, byte[]> tree;
    static LinkedHashMap<Integer, byte[]> lhm;
    static LinkedList<byte[]> list;
    static volatile Object sink;

    static byte[] fill(int bytes, int seed) {
        byte[] b = new byte[bytes];
        for (int k = 0; k < bytes; k++) {
            b[k] = (byte) (seed * 31 + k);
        }
        return b;
    }

    /** Sampled content check: returns the number of wrong bytes seen. */
    static int check(byte[] b, int bytes, int seed) {
        if (b == null || b.length != bytes) {
            return 1;
        }
        int bad = 0;
        for (int k = 0; k < bytes; k += 4093) {
            if (b[k] != (byte) (seed * 31 + k)) {
                bad++;
            }
        }
        if (b[bytes - 1] != (byte) (seed * 31 + bytes - 1)) {
            bad++;
        }
        return bad;
    }

    static long sampleSum(byte[] b) {
        long s = 0;
        if (b == null) {
            return s;
        }
        for (int k = 0; k < b.length; k += 4093) {
            s += b[k];
        }
        return s + b[b.length - 1];
    }

    static void churn(int mb) {
        Object[] ring = new Object[64];
        long n = (long) mb * 1024L;
        for (long i = 0; i < n; i++) {
            ring[(int) (i & 63)] = new byte[1000];
        }
        sink = ring;
    }

    public static void main(String[] a) {
        final int values = Integer.parseInt(a[0]);
        final int bytes = Integer.parseInt(a[1]);
        final int rounds = Integer.parseInt(a[2]);
        final int churnMb = Integer.parseInt(a[3]);
        long t0 = System.nanoTime();
        tree = new TreeMap<>();
        lhm = new LinkedHashMap<>();
        list = new LinkedList<>();
        for (int i = 0; i < values; i++) {
            tree.put(i, fill(bytes, i));
            lhm.put(i, fill(bytes, 1000 + i));
            list.add(fill(bytes, 2000 + i));
        }
        long bad = 0;
        for (int r = 0; r < rounds; r++) {
            // Allocation only: the collections are not written again, so a
            // collector that judges them by "written since the last pause"
            // has nothing to go on after their promotion.
            churn(churnMb);
            for (int i = 0; i < values; i++) {
                bad += check(tree.get(i), bytes, i);
                bad += check(lhm.get(i), bytes, 1000 + i);
            }
            int j = 0;
            for (byte[] b : list) {
                bad += check(b, bytes, 2000 + j);
                j++;
            }
        }
        long sum = 0;
        for (Map.Entry<Integer, byte[]> e : tree.entrySet()) {
            sum += sampleSum(e.getValue());
        }
        for (Map.Entry<Integer, byte[]> e : lhm.entrySet()) {
            sum += sampleSum(e.getValue());
        }
        for (byte[] b : list) {
            sum += sampleSum(b);
        }
        System.out.println("cd44 values=" + values + " bytes=" + bytes + " rounds=" + rounds
                + " tree=" + tree.size() + " lhm=" + lhm.size() + " list=" + list.size()
                + " bad=" + bad + " sum=" + sum);
        System.err.println("[gci-cd44] ms=" + (System.nanoTime() - t0) / 1_000_000);
    }
}
