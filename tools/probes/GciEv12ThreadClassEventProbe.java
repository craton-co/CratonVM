// gci wave i12, lane ev: GC-safe JVMTI calls from ThreadStart / ThreadEnd /
// ClassLoad / ClassPrepare callbacks
// (docs/known-issues/gc/gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-20261013.md,
// item 1c).
//
// Mode `<lib> <workers>` (needs -agentpath:<lib>, built from
// tools/probes/jni/GciEv12ThreadClassEventProbe.c, and a VM that runs C JVMTI
// agents): with the four events enabled, `workers` threads named `ev12-w<n>`
// run four at a time; worker n < 16 loads and initializes class `GciEv12G<n>`
// (Class.forName). Each worker's ThreadStart / ThreadEnd callback, and each
// `GciEv12G*` ClassLoad / ClassPrepare callback, enters a raw monitor and
// waits on it for 1 ms (a GC-safe wait); every 8th worker thread event also
// forces a collection. A daemon thread requests collections back to back
// meanwhile. Each worker holds an object across its events; a stale or lost
// root shows as `heldBad` or a broken chain.
//
// stdout is deterministic (HotSpot 25 is the reference); diagnostics go to
// stderr: `[ev12] gcs=<n> ...`, `[ev12] lambdaLoads=<n> lambdaPrepares=<n> jdkLoads=<n>`.
// Workers 16..19 each build a ConcurrentSkipListSet (a bootstrap load for
// the first of them; names / heldBad unchanged).
//
//   javac -d <classes> tools/probes/GciEv12ThreadClassEventProbe.java
//   java -agentpath:<dir>/gciev12tc.dll -cp <classes> GciEv12ThreadClassEventProbe <dir>/gciev12tc.dll 64
public class GciEv12ThreadClassEventProbe {
    static native int init();

    static native int arm();

    static native int disarm();

    // 0 worker ThreadStarts, 1 worker ThreadEnds, 2 GciEv12G* ClassLoads,
    // 3 GciEv12G* ClassPrepares, 4 raw monitor errors, 5 ForceGarbageCollection
    // errors, 6 GetThreadInfo errors, 7 GetClassSignature errors, 8 / 9 the
    // probe's lambda ClassLoads / ClassPrepares, 10 java.util.concurrent.
    // ConcurrentSkipList* ClassLoads.
    static native long stat(int which);

    static final int CLASSES = 16;

    static final class Node {
        final int value;
        final Node next;

        Node(int value, Node next) {
            this.value = value;
            this.next = next;
        }
    }

    static volatile boolean stop;
    static Node keep;

    static Thread collector() {
        Thread gc = new Thread(() -> {
            long n = 0;
            byte[] sink = null;
            while (!stop) {
                sink = new byte[1 << 16];
                System.gc();
                n++;
            }
            System.err.println("[ev12] gcs=" + n + " last=" + (sink == null ? 0 : sink.length));
        }, "ev12-gc");
        gc.setDaemon(true);
        return gc;
    }

    static Node chain(int n) {
        Node head = null;
        for (int i = 0; i < n; i++) {
            head = new Node(i, head);
        }
        return head;
    }

    static long chainSum(Node head) {
        long sum = 0;
        for (Node n = head; n != null; n = n.next) {
            sum += n.value;
        }
        return sum;
    }

    static void work(int idx, long[] names, long[] bad) {
        Node held = new Node(-1, keep);
        long n = 0;
        if (idx < CLASSES) {
            try {
                n += Class.forName("GciEv12G" + idx).getName().length();
            } catch (ClassNotFoundException e) {
                n -= 1000;
            }
        }
        if (idx >= CLASSES && idx < CLASSES + 4) {
            // A bootstrap load on this worker (stderr `jdkLoads`): the VM's
            // thread-less load path, deferred under
            // CRATONVM_JVMTI_DEFER_UNOWNED_CLASS_LOAD=1.
            java.util.concurrent.ConcurrentSkipListSet<Integer> set =
                    new java.util.concurrent.ConcurrentSkipListSet<>();
            set.add(idx);
            n += set.size() - 1;
        }
        for (int j = 0; j < 200; j++) {
            Node t = new Node(j, held);
            n += t.value - j;
        }
        if (held.next != keep || held.value != -1) {
            bad[idx]++;
        }
        names[idx] = n;
    }

    public static void main(String[] args) throws Exception {
        System.load(new java.io.File(args[0]).getAbsolutePath());
        final int workers = Integer.parseInt(args[1]);
        System.out.println("init " + init());
        keep = chain(2000);
        Thread gc = collector();
        gc.start();
        final long[] names = new long[workers];
        final long[] bad = new long[workers];
        int armed = arm();
        for (int base = 0; base < workers; base += 4) {
            Thread[] batch = new Thread[Math.min(4, workers - base)];
            for (int k = 0; k < batch.length; k++) {
                final int idx = base + k;
                batch[k] = new Thread(() -> work(idx, names, bad), "ev12-w" + idx);
                batch[k].start();
            }
            for (Thread t : batch) {
                t.join();
            }
        }
        int disarmed = disarm();
        stop = true;
        gc.join();
        long nameSum = 0;
        long heldBad = 0;
        for (int i = 0; i < workers; i++) {
            nameSum += names[i];
            heldBad += bad[i];
        }
        System.out.println("arm " + armed + " disarm " + disarmed);
        System.out.println("workers " + workers + " names " + nameSum + " heldBad " + heldBad);
        System.out.println("threadStarts " + stat(0) + " threadEnds " + stat(1) + " classLoads " + stat(2)
                + " classPrepares " + stat(3));
        System.out.println("monitorErr " + stat(4) + " forceErr " + stat(5) + " infoErr " + stat(6)
                + " sigErr " + stat(7));
        System.out.println("chain 2000 sum " + chainSum(keep));
        System.err.println("[ev12] lambdaLoads=" + stat(8) + " lambdaPrepares=" + stat(9)
                + " jdkLoads=" + stat(10));
    }
}

class GciEv12G0 { static int v = 0; }

class GciEv12G1 { static int v = 1; }

class GciEv12G2 { static int v = 2; }

class GciEv12G3 { static int v = 3; }

class GciEv12G4 { static int v = 4; }

class GciEv12G5 { static int v = 5; }

class GciEv12G6 { static int v = 6; }

class GciEv12G7 { static int v = 7; }

class GciEv12G8 { static int v = 8; }

class GciEv12G9 { static int v = 9; }

class GciEv12G10 { static int v = 10; }

class GciEv12G11 { static int v = 11; }

class GciEv12G12 { static int v = 12; }

class GciEv12G13 { static int v = 13; }

class GciEv12G14 { static int v = 14; }

class GciEv12G15 { static int v = 15; }
