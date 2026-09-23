// Ten-findings item 1, the discriminating shape: a LARGE retained old
// generation (so a whole-heap fix-up walk is expensive), ONE humongous span
// held by it (which, before item 1, forced every young pause to take that
// whole-heap walk to build its census), and a young churn that never touches
// the old set (so a narrow walk has almost nothing to do).
//
// The checksum reads through both the retained set and the span, so a wrongly
// reclaimed region is a wrong number or a crash rather than a faster run.
public class HumongousWide {
    static final class Node {
        Node next;
        long[] payload;
    }

    public static void main(String[] args) {
        int liveMiB = args.length > 0 ? Integer.parseInt(args[0]) : 64;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 400;

        // One humongous span, retained for the whole run.
        long[] big = new long[600_000]; // 4.8 MB
        for (int i = 0; i < big.length; i++) {
            big[i] = i * 3L;
        }

        // A retained chain that ages into Old: ~72 bytes per node with its
        // payload, so liveMiB * 14563 nodes is roughly liveMiB megabytes.
        int nodes = liveMiB * 14_563;
        Node head = null;
        for (int i = 0; i < nodes; i++) {
            Node n = new Node();
            n.payload = new long[4];
            n.payload[0] = i;
            n.next = head;
            head = n;
        }

        // Young churn that touches nothing old. Every pause here is the one
        // item 1 is about: a small collection set, a large untouched old set,
        // and a humongous span sitting in the heap.
        long checksum = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < 8_000; i++) {
                long[] junk = new long[32];
                junk[0] = i;
                checksum += junk[0] & 1;
            }
            checksum += big[(r * 7919) % big.length];
        }

        // Prove the retained set survived, and read enough of it that a
        // collector which dropped part of it cannot produce this number.
        long walk = 0;
        Node n = head;
        while (n != null) {
            walk += n.payload[0];
            n = n.next;
        }
        System.out.println("checksum=" + (checksum + walk));
    }
}
