// Recreated standalone repro for the G1 parallel evacuation residual race.
// The original scratch/g1par source was not tracked, so this is not byte-identical.
//
// Shape:
//   * maintain a fixed 4096-node live linked list reachable from one root;
//   * replace one node per iteration so the live set remains young/survivor-heavy;
//   * give every live node a payload object and byte[] so the 16m heap forces
//     to-space pressure and evacuation failure/self-forwarding;
//   * allocate short-lived payloads each iteration to keep young GC frequent.
//
// Correct command from the bug note:
//
//   CRATONVM_G1_PARALLEL_EVAC=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
//     cratonvm --nojit -XX:+UseG1GC -Xmx16m -cp <classes> SteadyChurn 2000000
//
// Expected stdout for 2,000,000 iterations: 2002062093760
public final class SteadyChurn {
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
        for (int i = 1; i < LIVE; i++) {
            Node node = new Node(i);
            tail.next = node;
            tail = node;
        }

        long churn = 0;
        for (int i = 0; i < iterations; i++) {
            Node node = new Node(LIVE + i);
            tail.next = node;
            tail = node;
            head = head.next;

            Payload a = new Payload(i ^ 0x5a5a5a5a);
            Payload b = new Payload(i ^ 0x33cc33cc);
            a.owner = head;
            b.owner = tail;

            churn += (head.seq & 31)
                    + (tail.seq & 17)
                    + (a.bytes[0] & 0xff)
                    + (b.bytes[b.bytes.length - 1] & 0xff);
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
            check += node.seq * 17L;
            count++;
            node = node.next;
        }
        if (count != LIVE) {
            fail();
        }
        int firstExpected = iterations;
        if (head.seq != firstExpected) {
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
