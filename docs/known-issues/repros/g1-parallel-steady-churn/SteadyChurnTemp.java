// Regression repro for INTERPRETER LOCAL LIVENESS (runtime/local_liveness.rs).
//
// Identical to SteadyChurnLight except the list-setup loop deliberately uses
// a construction temp:
//
//     Node node = new Node(i);   // scoped-out after the setup loop
//     tail.next = node;
//     tail = node;
//
// Under a liveness-free root scan (CRATONVM_NO_LOCAL_LIVENESS=1) the temp's
// slot keeps node[LIVE-1] alive for main's whole lifetime and, through the
// `next` chain, EVERY node appended afterwards — unbounded retention that
// OOMs at any heap size. With the per-bci liveness filter the temp is dead
// after the setup loop and the run completes at -Xmx16m like HotSpot.
//
//   CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
//     cratonvm --nojit -XX:+UseG1GC -Xmx16m -cp <classes> SteadyChurnTemp 2000000
//
// Expected stdout for 2,000,000 iterations: 2002062093760
public final class SteadyChurnTemp {
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
            Node node = new Node(i); // THE deliberate scoped-out temp
            tail.next = node;
            tail = node;
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
