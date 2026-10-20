// Ten-findings item 1, the discriminating shape — and one that actually
// pauses. `HumongousWide`'s inner churn was dead on arrival and the JIT
// removed it, so the run took three pauses and measured nothing.
//
// Here the churn escapes into a rotating window, so every round genuinely
// allocates and the collector genuinely runs. The shape:
//
//   * a LARGE retained old generation (a whole-heap fix-up walk is expensive);
//   * ONE humongous span held by it — before item 1 that forced every young
//     pause to take the whole-heap walk to build its eager-reclaim census;
//   * a young churn that never touches the retained set, so a narrow walk has
//     almost nothing to do.
//
// The checksum reads through the retained set, the window and the span.
public class HumongousChurn {
    static final class Node {
        Node next;
        long[] payload;
    }

    public static void main(String[] args) {
        int liveMiB = args.length > 0 ? Integer.parseInt(args[0]) : 48;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 600;
        int window = args.length > 2 ? Integer.parseInt(args[2]) : 512;

        long[] big = new long[600_000]; // 4.8 MB — humongous at any region size
        for (int i = 0; i < big.length; i++) {
            big[i] = i * 3L;
        }

        int nodes = liveMiB * 14_563;
        Node head = null;
        for (int i = 0; i < nodes; i++) {
            Node n = new Node();
            n.payload = new long[4];
            n.payload[0] = i;
            n.next = head;
            head = n;
        }

        // The rotating window: each slot is overwritten `rounds*64/window`
        // times, so the arrays are real, short-lived, and reachable long
        // enough that the collector has to copy some of them.
        long[][] live = new long[window][];
        long checksum = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < 64; i++) {
                long[] a = new long[64];
                a[0] = r * 31L + i;
                live[(r * 64 + i) % window] = a;
                checksum += a[0] & 3;
            }
            checksum += big[(r * 7919) % big.length];
        }

        for (long[] a : live) {
            if (a != null) {
                checksum += a[0] & 7;
            }
        }
        long walk = 0;
        Node n = head;
        while (n != null) {
            walk += n.payload[0];
            n = n.next;
        }
        System.out.println("checksum=" + (checksum + walk));
    }
}
