// A workload for measuring G1's ROOT-COVERAGE rate, built because nothing
// existing could be measured against.
//
// The problem it solves (audit §18.2, §20.3): `org.h2.test.store.TestMVStoreTool`
// exercises 612 compiled frames — enough to see the coverage obligations — but
// its incomplete rate swings from 2.9% to 83.6% on an UNCHANGED binary, and its
// pause count from 14 to 243. Nothing worth a few percent can be measured
// against that. The probes at the other end (`HumongousChurn` and friends) are
// stable but report 0% incomplete, because `main` plus a couple of helpers is
// not enough compiled frame to have an obligation about.
//
// What this needs, therefore, and what the shape below is for:
//
//   * MANY DISTINCT HOT METHODS, each holding several REFERENCE LOCALS live
//     across an allocating call. That is what puts oops in a compiled frame's
//     spill band at a safepoint, which is the thing the coverage verifier has
//     an opinion about. One method with a deep loop gives one frame; twenty
//     methods called in rotation give twenty.
//
//   * DETERMINISM. No clock, no IO, no hashing of identities, no thread
//     scheduling. Every allocation, every call and every pause boundary is a
//     function of the arguments alone, so two runs of the same arguments do
//     the same work in the same order.
//
//   * ENOUGH GARBAGE to force many pauses at a small heap, with a live set that
//     stays constant so the pause POSITIONS are stable too — a retained set
//     that grows would move every pause boundary as the run proceeds.
//
// The checksum reads through the retained set, the rotating window and every
// method's result, so a collector that drops or fails to rewrite a reference
// produces a wrong number rather than a faster run.
public class CoverageBench {

    static final class Node {
        Node next;
        long[] payload;
        String tag;
    }

    // A fixed-size retained set. Constant across the run: the live set must not
    // grow, or successive pauses see a different heap and the rate drifts
    // within a single run.
    static Node[] retained;
    static long[][] window;

    // --- the hot methods -------------------------------------------------
    //
    // Each holds 3+ reference locals across `alloc`, which is the allocating
    // (and therefore safepoint-bearing) call. They differ in body so the JIT
    // compiles them separately rather than sharing one profile.

    static long[] alloc(int n) {
        long[] a = new long[n];
        a[0] = n;
        return a;
    }

    static long m00(Node n, int i) {
        long[] a = alloc(24); Node p = n.next; String t = n.tag;
        long[] b = alloc(8);
        return a[0] + b[0] + (p == null ? 1 : p.payload.length) + t.length();
    }
    static long m01(Node n, int i) {
        String t = n.tag; long[] a = alloc(16); Node p = n.next;
        long[] b = alloc(32);
        return a[0] - b[0] + t.length() + (p == null ? 3 : 2);
    }
    static long m02(Node n, int i) {
        Node p = n.next; long[] a = alloc(12); String t = n.tag; long[] c = alloc(4);
        return a[0] + c[0] + t.length() + (p == null ? 5 : p.payload.length);
    }
    static long m03(Node n, int i) {
        long[] a = alloc(40); String t = n.tag; Node p = n.next; long[] d = alloc(2);
        return a[0] * 2 + d[0] + t.length() + (p == null ? 7 : 1);
    }
    static long m04(Node n, int i) {
        Node p = n.next; String t = n.tag; long[] a = alloc(20);
        long[] b = alloc(20); long[] c = alloc(6);
        return a[0] + b[0] + c[0] + t.length() + (p == null ? 11 : 0);
    }
    static long m05(Node n, int i) {
        long[] a = alloc(9); Node p = n.next; long[] b = alloc(15); String t = n.tag;
        return a[0] ^ b[0] + t.length() + (p == null ? 13 : p.payload.length);
    }
    static long m06(Node n, int i) {
        String t = n.tag; Node p = n.next; long[] a = alloc(48);
        return a[0] + t.length() * 2 + (p == null ? 17 : 4);
    }
    static long m07(Node n, int i) {
        long[] a = alloc(7); long[] b = alloc(11); Node p = n.next; String t = n.tag;
        return a[0] + b[0] + (p == null ? 19 : p.payload.length) + t.length();
    }
    static long m08(Node n, int i) {
        Node p = n.next; long[] a = alloc(28); String t = n.tag; long[] b = alloc(3);
        return a[0] - b[0] + t.length() + (p == null ? 23 : 6);
    }
    static long m09(Node n, int i) {
        long[] a = alloc(5); String t = n.tag; long[] b = alloc(36); Node p = n.next;
        return a[0] + b[0] / 2 + t.length() + (p == null ? 29 : 8);
    }

    static long dispatch(int k, Node n, int i) {
        switch (k) {
            case 0: return m00(n, i);
            case 1: return m01(n, i);
            case 2: return m02(n, i);
            case 3: return m03(n, i);
            case 4: return m04(n, i);
            case 5: return m05(n, i);
            case 6: return m06(n, i);
            case 7: return m07(n, i);
            case 8: return m08(n, i);
            default: return m09(n, i);
        }
    }

    public static void main(String[] args) {
        int liveNodes = args.length > 0 ? Integer.parseInt(args[0]) : 20_000;
        int rounds    = args.length > 1 ? Integer.parseInt(args[1]) : 4_000;
        int windowSz  = args.length > 2 ? Integer.parseInt(args[2]) : 512;

        retained = new Node[liveNodes];
        Node head = null;
        for (int i = 0; i < liveNodes; i++) {
            Node n = new Node();
            n.payload = new long[4];
            n.payload[0] = i;
            n.tag = "n" + (i % 97);   // a bounded set of distinct strings
            n.next = head;
            head = n;
            retained[i] = n;
        }
        window = new long[windowSz][];

        long checksum = 0;
        for (int r = 0; r < rounds; r++) {
            // Walk the retained set by index, not by chasing `next`, so the
            // access pattern does not depend on where the collector placed
            // anything.
            // `(long)` deliberately: at `int` width `r * 7919` overflows past
            // ~271k rounds and the index goes negative, which reads as a VM
            // ArrayIndexOutOfBounds rather than as the probe's own arithmetic.
            // Unchanged below that bound, so published checksums still hold.
            Node n = retained[(int) (((long) r * 7919L) % liveNodes)];
            for (int k = 0; k < 10; k++) {
                checksum += dispatch(k, n, r);
            }
            // Escaping garbage: keeps the allocation real and gives the
            // collector something to copy.
            for (int i = 0; i < 32; i++) {
                long[] a = alloc(64);
                a[0] = r * 31L + i;
                window[(r * 32 + i) % windowSz] = a;
                checksum += a[0] & 3;
            }
        }

        for (long[] a : window) {
            if (a != null) checksum += a[0] & 7;
        }
        long walk = 0;
        for (Node n : retained) walk += n.payload[0] + n.tag.length();
        System.out.println("checksum=" + (checksum + walk));
    }
}
