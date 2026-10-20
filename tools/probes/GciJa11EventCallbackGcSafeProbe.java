// gci wave i11, lane ja: GC-safe JVMTI calls from agent event callbacks
// (docs/known-issues/gc/gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-20261013.md,
// items 1 and 2).
//
// Mode `<lib> <rounds>` (needs -agentpath:<lib>, built from
// tools/probes/jni/GciJa11EventCallbackGcSafeProbe.c, and a VM that runs C
// JVMTI agents): a worker thread calls `hook(i)` `rounds` times with the
// agent's MethodEntry and MethodExit (and, when the VM grants the capability, FramePop)
// enabled for that thread only. Each `hook` entry's callback enters a raw
// monitor and waits on it for 1 ms (a GC-safe wait), and every 20th forces a
// collection; each `hook` FramePop's callback waits the same way. A daemon
// thread requests collections back to back meanwhile, so collections run
// while the worker is parked inside the callbacks. The worker's frames hold
// objects across every call; a stale or lost root shows as a wrong sum or a
// broken chain.
//
// Mode `exit`: no agent. The program prints, then calls System.exit(3) while
// a daemon thread allocates and requests collections (the exit natives'
// pre-exit hook, item 2, with and without CRATONVM_EXIT_HOOK_GC_SAFE).
//
// stdout is deterministic (HotSpot 25 is the reference); diagnostics go to
// stderr: `[ja11] gcs=<n>`, `[ja11] framePops=<n> notifyErr=<n> cap=<0|1>`.
//
//   javac -d <classes> tools/probes/GciJa11EventCallbackGcSafeProbe.java
//   java -agentpath:<dir>/gcija11cb.dll -cp <classes> GciJa11EventCallbackGcSafeProbe <dir>/gcija11cb.dll 400
//   java -cp <classes> GciJa11EventCallbackGcSafeProbe exit
public class GciJa11EventCallbackGcSafeProbe {
    static native int init(Class<?> self);

    static native int arm(Thread t);

    static native int disarm(Thread t);

    // 0 hook entries, 1 raw monitor errors, 2 ForceGarbageCollection errors,
    // 3 hook FramePops, 4 NotifyFramePop errors, 5 FramePop capability (0/1),
    // 6 hook MethodExits.
    static native long stat(int which);

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

    static int hook(int i) {
        Node local = new Node(i, null);
        int[] pad = new int[64];
        pad[i & 63] = i;
        return local.value + pad[i & 63];
    }

    static Thread collector() {
        Thread gc = new Thread(() -> {
            long n = 0;
            byte[] sink = null;
            while (!stop) {
                sink = new byte[1 << 16];
                System.gc();
                n++;
            }
            System.err.println("[ja11] gcs=" + n + " last=" + (sink == null ? 0 : sink.length));
        }, "ja11-gc");
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

    public static void main(String[] args) throws Exception {
        if (args.length == 1 && args[0].equals("exit")) {
            keep = chain(2000);
            Thread gc = collector();
            gc.start();
            for (int i = 0; i < 2000; i++) {
                keep = new Node(keep.value, keep.next);
            }
            System.out.println("exit chain 2000 sum " + chainSum(keep));
            System.out.flush();
            System.exit(3);
        }
        System.load(new java.io.File(args[0]).getAbsolutePath());
        final int rounds = Integer.parseInt(args[1]);
        System.out.println("init " + init(GciJa11EventCallbackGcSafeProbe.class));
        keep = chain(2000);
        final long[] out = new long[4];
        Thread worker = new Thread(() -> {
            Node held = new Node(-1, keep);
            out[0] = arm(Thread.currentThread());
            long sum = 0;
            long heldBad = 0;
            for (int i = 0; i < rounds; i++) {
                sum += hook(i);
                if (held.next != keep || held.value != -1) {
                    heldBad++;
                }
            }
            out[1] = disarm(Thread.currentThread());
            out[2] = sum;
            out[3] = heldBad;
        }, "ja11-worker");
        Thread gc = collector();
        gc.start();
        worker.start();
        worker.join();
        stop = true;
        gc.join();
        System.out.println("arm " + out[0] + " disarm " + out[1]);
        System.out.println("hooks " + rounds + " sum " + out[2] + " expected " + ((long) rounds * (rounds - 1)));
        System.out.println("entries " + stat(0) + " monitorErr " + stat(1) + " forceErr " + stat(2));
        System.out.println("chain 2000 sum " + chainSum(keep) + " heldBad " + out[3]);
        System.err.println("[ja11] framePops=" + stat(3) + " notifyErr=" + stat(4) + " cap=" + stat(5)
                + " exits=" + stat(6));
    }
}
