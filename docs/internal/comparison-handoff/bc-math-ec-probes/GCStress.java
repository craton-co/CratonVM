import java.util.Random;

// Generic allocation-heavy probe that mirrors the BC SecT field-element pattern
// WITHOUT any EC code: objects carrying a small long[] field, kept across many
// allocations (so they promote to old gen), with old->young field stores and
// heavy short-lived garbage. Run interpreted (CRATONVM_DISABLE_JIT=1) under
// CRATONVM_DBG_GC_STRESS to see if the 0-slot-Object stale-ref corruption
// reproduces outside BouncyCastle EC.
public class GCStress {
    static final class Node {
        long[] data;
        Node next;
        int x;
    }

    public static void main(String[] a) {
        int iters = a.length > 0 ? Integer.parseInt(a[0]) : 2_000_000;
        Node[] kept = new Node[64];
        long check = 0;
        for (int i = 0; i < iters; i++) {
            Node n = new Node();
            n.data = new long[7];                 // ~SecT193 limb count
            // deterministic fill (no java.util.Random) so the final sum is a
            // pure correctness oracle: any divergence from HotSpot == corruption.
            for (int j = 0; j < 7; j++) n.data[j] = 0x9E3779B97F4A7C15L * (i + 1) + j;
            n.x = i;
            kept[i & 63] = n;                     // hold across allocations -> old gen
            // heavy short-lived garbage between holding the ref and mutating it
            for (int g = 0; g < 12; g++) {
                long[] junk = new long[7];
                junk[0] = (long) g * i;
                if (junk[6] != 0) check ^= junk[0];   // keep junk from being elided
            }
            // old->young store + field mutation on a promoted node (write barrier path)
            Node m = kept[(i * 7) & 63];
            if (m != null) {
                m.next = n;                       // old.ref = young
                m.x = m.x + 1;
                if (m.data != null && m.data.length == 7) m.data[0] ^= (i + 1);
            }
            if ((i % 100_000) == 0) { System.err.println("i=" + i); System.err.flush(); }
        }
        long sum = 0;
        for (Node n : kept) if (n != null && n.data != null) sum += n.x + n.data[0];
        System.err.println("DONE iters=" + iters + " sum=" + sum + " check=" + check);
        System.err.flush();
    }
}
