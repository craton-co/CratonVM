// BUG-03 reproducer — multiple worker threads repeatedly call a hot, JIT-
// compiled static method `build` that allocates a linked list and keeps its
// head live in a JIT local across every allocation, then verifies the list.
// Under frequent GC (small --Xmx) several workers are inside `build`'s compiled
// code while a peer's allocation triggers a stop-the-world collection — the
// multi-thread-in-JIT-under-STW condition behind BUG-03. Each `build` result is
// checked against an independently computed expected value, so a dropped GC
// root (reclaimed node) surfaces as a wrong checksum ("CORRUPT").
//
//   flag OFF (default)                : may print CORRUPT / crash under stress
//   CRATONVM_XT_JIT_ROOT_SCAN=1 (fix) : correct every run; take-overs engage
//
// Run with a small heap for frequent natural GC, e.g.:
//   CRATONVM_XT_JIT_ROOT_SCAN=1 cratonvm --Xmx 24m -cp . XtJitRepro
public class XtJitRepro {
    static final int THREADS = 6;
    static final int ITERS = 120_000;
    static final int LIST_LEN = 64;
    static volatile long sink;

    static final class Node {
        int v;
        Node next;
    }

    // Hot, JIT-compiled. `head` stays live in a local across all allocations.
    static long build(int id, int iter) {
        Node head = null;
        for (int j = 0; j < LIST_LEN; j++) {
            Node n = new Node();
            n.v = id * 1000 + j + (iter & 0xFF);
            n.next = head;
            head = n;
        }
        long s = 0;
        Node c = head;
        while (c != null) {
            s += c.v;
            c = c.next;
        }
        return s;
    }

    static long expected(int id, int iter) {
        long s = 0;
        for (int j = 0; j < LIST_LEN; j++) {
            s += id * 1000 + j + (iter & 0xFF);
        }
        return s;
    }

    public static void main(String[] args) throws Exception {
        Thread[] ts = new Thread[THREADS];
        final boolean[] bad = new boolean[THREADS];
        for (int i = 0; i < THREADS; i++) {
            final int id = i;
            ts[i] = new Thread(() -> {
                long acc = 0;
                for (int iter = 0; iter < ITERS; iter++) {
                    long got = build(id, iter);
                    long exp = expected(id, iter);
                    if (got != exp) {
                        bad[id] = true;
                        System.out.println("CORRUPT id=" + id + " iter=" + iter
                                + " got=" + got + " exp=" + exp);
                    }
                    acc += got;
                }
                sink = acc;
                System.out.println("worker " + id + " done acc=" + acc);
            });
            ts[i].start();
        }
        for (Thread t : ts) t.join();
        boolean anyBad = false;
        for (boolean b : bad) anyBad |= b;
        System.out.println(anyBad ? "RESULT: CORRUPTION DETECTED" : "RESULT: OK");
    }
}
