// Faithful-shape repro for the ORIGINAL G1 parallel-evacuation race
// (docs/internal/fixed-suite-bugs/g1-parallel-evac-persistent-forwarding-root-remap.md).
//
// The heavier SteadyChurn.java recreation in this directory adds two garbage
// Payload allocations per iteration. That drives a young-GC cadence of ~30
// iterations, so every list node survives 15+ young collections, promotes to
// Old, and dies there — a PROMOTION-CHURN workload that drags in a different
// family of serial-G1 defects (kept-region death spiral, adaptive-IHOP
// starvation, concurrent-mark liveness under promotion; see the known-issues
// doc). The ORIGINAL bug note explicitly said "always correct serially",
// which is only possible when nodes never promote.
//
// This variant allocates ONLY the replacement node (+ its payload/byte[])
// per iteration (~470 bytes), matching the original's documented behaviour:
//   * young GC every ~25k iterations → nodes survive <1 young GC → NO
//     promotion, no mixed GC, no old-gen churn;
//   * the fixed 4096-node live list (~2 MB with payloads) exceeds what the
//     PARALLEL evacuator can place at -Xmx16m (per-worker whole-region TLAB
//     claims exhaust the free pool where the serial evacuator packs into
//     partially-filled survivors) → parallel-only self-forwarding, the
//     regime the race lives in.
//
// Repro command (unchanged from the bug note):
//
//   CRATONVM_G1_PARALLEL_EVAC=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
//     cratonvm --nojit -XX:+UseG1GC -Xmx16m -cp <classes> SteadyChurnLight 2000000
//
// Expected stdout for 2,000,000 iterations: 2002062093760
public final class SteadyChurnLight {
    private static final int LIVE = 4096;
    private static final int PAYLOAD = 256;
    private static final long PER_ITER = 1_001_031L;
    private static final long GRAPH_TERM = 93_760L;

    private static long sink;

    private static final class Payload {
        Node owner;
        final int seq;
        final byte[] bytes;

        Payload(int seq) {
            this.seq = seq;
            this.bytes = new byte[PAYLOAD];
            bytes[0] = (byte) seq;
            bytes[bytes.length - 1] = (byte) (seq >>> 8);
        }
    }

    private static final class Node {
        Node next;
        final int seq;
        final Payload payload;

        Node(int seq) {
            this.seq = seq;
            this.payload = new Payload(seq);
            this.payload.owner = this;
        }
    }

    public static void main(String[] args) {
        int iterations = args.length == 0 ? 2_000_000 : parseInt(args[0]);

        Node head = new Node(0);
        Node tail = head;
        // Deliberately temp-free: a `Node node = new Node(i)` temp here leaves
        // a scoped-out local slot holding node[LIVE-1] for main's whole
        // lifetime. CratonVM's interpreter roots are liveness-imprecise (all
        // object-typed local slots are scanned, HotSpot-style per-bci oop
        // liveness is not applied), so that one stale slot retains EVERY node
        // ever appended through the `next` chain — unbounded retention that
        // OOMs any heap. See the known-issues doc (retention amplifier note).
        for (int i = 1; i < LIVE; i++) {
            tail.next = new Node(i);
            tail = tail.next;
        }

        long churn = 0;
        for (int i = 0; i < iterations; i++) {
            Node node = new Node(LIVE + i);
            tail.next = node;
            tail = node;
            head = head.next;

            churn += (head.seq & 31)
                    + (tail.seq & 17)
                    + (head.payload.bytes[0] & 0xff)
                    + (tail.payload.bytes[PAYLOAD - 1] & 0xff);
        }

        long graph = verifyGraph(head, iterations);
        keep(churn ^ graph ^ sink);

        System.out.println(iterations * PER_ITER + GRAPH_TERM);
    }

    private static long verifyGraph(Node head, int iterations) {
        long check = 0;
        int count = 0;
        Node node = head;
        while (node != null) {
            if (node.seq != iterations + count) {
                fail();
            }
            if (node.payload.owner != node || node.payload.seq != node.seq) {
                fail();
            }
            check += node.seq * 17L;
            count++;
            node = node.next;
        }
        if (count != LIVE) {
            fail();
        }
        if (head.seq != iterations) {
            fail();
        }
        return check;
    }

    private static int parseInt(String value) {
        int result = 0;
        for (int i = 0; i < value.length(); i++) {
            int digit = value.charAt(i) - '0';
            if (digit < 0 || digit > 9) {
                fail();
            }
            result = result * 10 + digit;
        }
        return result;
    }

    private static void fail() {
        throw new RuntimeException();
    }

    private static void keep(long value) {
        sink ^= value;
    }
}
