// Does a skip span published by `Thread.getStackTrace()` and never retired get
// CONSUMED by a later collection?
//
// The leak needs three things to line up, and this drives all three:
//
//   1. a pause that PUBLISHES skip spans but does not retire them --
//      `stw_publish_frame_traces`, reached from ordinary Java by
//      `Thread.getStackTrace()` on a running peer;
//   2. the span's owner resuming and bump-allocating LIVE objects into what
//      was, at freeze time, its un-allocated tail; and
//   3. a later collection on a path that does not republish the set --
//      `maybe_gc`'s single-threaded fast path (`alive_count <= 1`), which is
//      why the worker has to be dead before the pressure phase.
//
// If the stale span is honoured, the sweep skips those objects and the mark
// oracle answers "gap space" for the static array's references to them, so
// they are neither scanned nor swept while everything they reference is freed.
public class SkipSpanRetireProbe {
    static final int KEEP = 20000;
    static final Object[] KEPT = new Object[KEEP];
    static volatile int kept = 0;
    static volatile boolean stop = false;

    static final class Node {
        int tag;
        Node next;
        Node(int tag, Node next) {
            this.tag = tag;
            this.next = next;
        }
    }

    public static void main(String[] args) throws Exception {
        Thread worker = new Thread(() -> {
            Node prev = null;
            int i = 0;
            while (!stop && kept < KEEP) {
                Node n = new Node(i, prev);
                prev = n;
                KEPT[i] = n;
                kept = i + 1;
                i++;
            }
        }, "alloc-worker");
        worker.start();

        // Hammer the stack-trace pause so at least one lands while the worker
        // holds a reserved TLAB tail. Each one publishes every alive thread's
        // tail; with the retire suppressed, the last one stays behind.
        int pauses = 0;
        for (int i = 0; i < 6000 && worker.isAlive(); i++) {
            worker.getStackTrace();
            pauses++;
        }
        stop = true;
        worker.join();

        // Single-threaded from here: every young collection now takes the
        // fast path that never republishes the skip list.
        long sink = 0;
        for (int i = 0; i < 600000; i++) {
            int[] junk = new int[64];
            junk[0] = i;
            sink += junk[0];
        }

        // Everything the worker published is reachable from KEPT, so all of it
        // must have survived intact.
        int bad = 0;
        int checked = 0;
        for (int i = 0; i < kept; i++) {
            try {
                Node n = (Node) KEPT[i];
                if (n == null || n.tag != i) {
                    bad++;
                } else {
                    checked++;
                }
            } catch (Throwable t) {
                bad++;
            }
        }
        System.out.println(
                "PROBE pauses=" + pauses + " kept=" + kept + " ok=" + checked + " bad=" + bad
                        + " sink=" + (sink != 0));
    }
}
