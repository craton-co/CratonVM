// zr z1/rl probe: real arrays past 256 MiB of payload on ZGC.
//
// Before the fix `ZgcRealHeap::alloc_size` refused any array whose payload
// exceeded 256 MiB (the object arm's byte cap, (1 << 24) * 16-byte slots), so such an array was
//   * LEAKED when dead (the sweep keeps an unsizable base forever),
//   * SKIPPED by the slide's reference-rewrite pass while live
//     ("zgc relocate: rewrite target is not walkable"), leaving every slot of a
//     big Object[] that named a moved object dangling.
// The RJdkBigNum guard lines were the first case: an int[67108865]
// (BigInteger.MAX_MAG_LENGTH + 1 ints) allocated on an overflow path.
//
// Rows (all stdout must equal `java -XX:+UseZGC -Xmx1g Zr1RlBigArrays <row>`):
//   leak     8 x dead int[(1<<26)+1]  (256 MiB + 4 each)   -> "leak ok 28"
//   dangle   live Object[(1<<25)+1] (256 MiB + 8) holding small objects that the slide moves,
//            checked after churn + System.gc()             -> "dangle ok"
public class Zr1RlBigArrays {
    public static void main(String[] args) {
        String row = args.length > 0 ? args[0] : "all";
        if (row.equals("leak") || row.equals("all")) leak();
        if (row.equals("dangle") || row.equals("all")) dangle();
    }

    static int sink;

    static void leak() {
        for (int i = 0; i < 8; i++) {
            int[] a = new int[(1 << 26) + 1];
            a[a.length - 1] = i;
            sink += a[a.length - 1];
            a = null;
            System.gc();
        }
        System.out.println("leak ok " + sink);
    }

    static final class Cell {
        final int id;
        final long check;
        Cell(int id) { this.id = id; this.check = ~((long) id * 0x9E3779B97F4A7C15L); }
        boolean ok(int i) { return id == i && check == ~((long) i * 0x9E3779B97F4A7C15L); }
    }

    static void dangle() {
        Object[] big = new Object[(1 << 25) + 1];
        final int stride = 8192;
        // Interleave the cells with garbage so their pages are mostly dead and
        // the relocation-set selector picks them; the slide then moves cells.
        for (int i = 0; i < big.length; i += stride) {
            for (int g = 0; g < 32; g++) {
                byte[] garbage = new byte[256];
                sink += garbage.length;
            }
            big[i] = new Cell(i);
        }
        for (int round = 0; round < 6; round++) {
            for (int g = 0; g < 200_000; g++) {
                byte[] garbage = new byte[128];
                sink += garbage.length & 1;
            }
            System.gc();
        }
        int bad = 0;
        int seen = 0;
        for (int i = 0; i < big.length; i += stride) {
            Object o = big[i];
            seen++;
            if (!(o instanceof Cell) || !((Cell) o).ok(i)) bad++;
        }
        System.out.println(bad == 0 ? "dangle ok " + seen : "dangle BAD " + bad + "/" + seen);
    }
}
