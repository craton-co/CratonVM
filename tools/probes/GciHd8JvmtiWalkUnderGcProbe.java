// gci wave i8, lane hd: the C JVMTI heap walks (`IterateThroughHeap`,
// `FollowReferences`) while another thread asks for full collections back to
// back and a third allocates
// (docs/known-issues/gc/gci-i3hd-heap-walks-and-diagnostics-review-findings-20261012.md, item 7).
//
// CratonVM runs each walk in a non-moving pause (`walking_from`,
// vm/src/jvmti/native_env/heap.rs). When a collection owned the world the
// request was refused and the walk used to proceed UNPAUSED, its callbacks
// reading a heap the collection was moving. The fix waits each refusal out
// GC-safe and asks again (JVMTI_ERROR_INTERNAL after 10 s).
//
// A chain of 1000 `Node`s is the only population of that class. Each round
// counts the Nodes `IterateThroughHeap` reports with the class filter `Node`
// (1000 expected) and the Nodes `FollowReferences` reaches from the head,
// following only Nodes (999: the initial object is not reported). Then the
// chain is checked from Java.
//
// Needs a CratonVM built with the vm crate's `experimental-debug` feature
// (the C jvmtiEnv) and the shim tools/probes/jni/GciHd8JvmtiWalkUnderGcProbe.c,
// loaded as an agent AND with System.load (args[0]) for the natives:
//
//   cl /LD /O1 /I"%JDK%\include" /I"%JDK%\include\win32" GciHd8JvmtiWalkUnderGcProbe.c /Fegcihd8walk.dll
//   javac -d <classes> tools/probes/GciHd8JvmtiWalkUnderGcProbe.java
//   java     -XX:+UseSerialGC -Xmx64m -agentpath:<dll> -cp <classes> GciHd8JvmtiWalkUnderGcProbe <dll> 100
//   cratonvm -Xmx64m -agentpath:<dll> -cp <classes> GciHd8JvmtiWalkUnderGcProbe <dll> 100
//
// Expected stdout (HotSpot 25; deterministic):
//
//   init 0
//   walks 100 nodes 1000 badThrough 0 badFollow 0 firstBad 0
//   chain 1000 sum 499500
//
// stderr (diagnostics only): `[hd8w] gcs=<System.gc calls> walk_max_ms=<n>`.
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

public class GciHd8JvmtiWalkUnderGcProbe {
    public static class Node {
        public Node next;
        public int value;

        public Node(int value) {
            this.value = value;
        }
    }

    /** `SetTag(nodeClass, 7)`'s error. */
    static native int init(Class<?> nodeClass);

    /** `IterateThroughHeap` with the class filter: the count, or `-error`. */
    static native long through(Class<?> nodeClass);

    /** `FollowReferences` from `initial`, following Nodes only: the Nodes reached, or `-error`. */
    static native long follow(Object initial, long mark);

    static Node head;
    static final Object[] RING = new Object[256];

    public static void main(String[] args) throws Exception {
        System.load(args[0]);
        int walks = args.length > 1 ? Integer.parseInt(args[1]) : 100;
        int n = 1000;
        Node prev = null;
        for (int i = n - 1; i >= 0; i--) {
            Node x = new Node(i);
            x.next = prev;
            prev = x;
        }
        head = prev;
        System.out.println("init " + init(Node.class));

        AtomicBoolean stop = new AtomicBoolean();
        AtomicLong gcs = new AtomicLong();
        Thread gc = new Thread(() -> {
            while (!stop.get()) {
                System.gc();
                gcs.incrementAndGet();
            }
        }, "gcloop");
        Thread alloc = new Thread(() -> {
            int k = 0;
            while (!stop.get()) {
                RING[k & 255] = new byte[512];
                k++;
            }
        }, "alloc");
        gc.setDaemon(true);
        alloc.setDaemon(true);
        gc.start();
        alloc.start();

        int badThrough = 0;
        int badFollow = 0;
        long firstBad = 0;
        long maxNs = 0;
        for (int w = 1; w <= walks; w++) {
            long s = System.nanoTime();
            long a = through(Node.class);
            long b = follow(head, w);
            maxNs = Math.max(maxNs, System.nanoTime() - s);
            if (a != n) {
                badThrough++;
                if (firstBad == 0) {
                    firstBad = a == 0 ? Long.MIN_VALUE : a;
                }
            }
            if (b != n - 1) {
                badFollow++;
                if (firstBad == 0) {
                    firstBad = b == 0 ? Long.MIN_VALUE : b;
                }
            }
        }
        stop.set(true);
        gc.join();
        alloc.join();
        System.out.println("walks " + walks + " nodes " + n + " badThrough " + badThrough
                + " badFollow " + badFollow + " firstBad " + firstBad);
        long sum = 0;
        int len = 0;
        for (Node x = head; x != null; x = x.next) {
            sum += x.value;
            len++;
        }
        System.out.println("chain " + len + " sum " + sum);
        System.err.println("[hd8w] gcs=" + gcs.get() + " walk_max_ms=" + maxNs / 1_000_000);
    }
}
