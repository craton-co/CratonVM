import java.nio.ByteBuffer;
import java.nio.channels.Pipe;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * nio21 wave 17 L3: witness for a missed root of a thread BLOCKED under compiled frames
 * (docs/internal/fixed-bugs/r21-blocking-native-deposits-an-eager-jit-band-scan-FIXED-20261009.md).
 *
 * Each worker builds a fresh object graph held ONLY in the locals of a hot (compiled) method,
 * then blocks -- in a `Pipe` read (a blocking native) or in `BlockingQueue.take` (a park) --
 * while a churn thread forces collections and allocates garbage. After the wake it walks the
 * graph and checks every value. A root the blocked thread's deposit (or, under
 * `CRATONVM_GC_LAZY_BLOCKED_JIT_DEPOSIT=1`, the collector's deferred walk) failed to publish is
 * an object reclaimed or moved under a live compiled frame: a wrong sum, a zeroed node, a
 * ClassCastException or a crash. The feeder releases every worker each round.
 *
 * Output (conformance, deterministic on HotSpot):
 *   blocked-roots pipe rounds=<R> workers=<W> bad=0
 *   blocked-roots queue rounds=<R> workers=<W> bad=0
 *
 * Run (every collector, both modes, with and without the lazy switch):
 *   javac -d out tools/probes/Nio21BlockedRoots.java
 *   java -cp out Nio21BlockedRoots > hs.txt
 *   cratonvm --jdk-only   -cp out Nio21BlockedRoots > jo.txt
 *   cratonvm --compatible -cp out Nio21BlockedRoots > co.txt
 *   CRATONVM_GC_LAZY_BLOCKED_JIT_DEPOSIT=1 cratonvm --jdk-only -XX:+UseG1GC -cp out Nio21BlockedRoots
 * -Dnio21.rounds=N (default 600), -Dnio21.workers=N (default 4).
 */
public class Nio21BlockedRoots {
    static final class Node {
        final long value;
        final int[] payload;
        Node next;

        Node(long value, Node next) {
            this.value = value;
            this.next = next;
            this.payload = new int[] {(int) value, (int) (value >>> 32), (int) (value * 31)};
        }
    }

    interface Blocker {
        void block() throws Exception;
    }

    static volatile Object sink;

    /** The hot method: the graph lives only in its locals across the block. */
    static int round(Blocker blocker, long seed) throws Exception {
        Node head = null;
        for (int i = 0; i < 24; i++) {
            head = new Node(seed * 1_000_003L + i, head);
        }
        String label = "r" + seed;
        long[] extra = new long[8];
        for (int i = 0; i < extra.length; i++) {
            extra[i] = seed ^ (i * 0x9E3779B97F4A7C15L);
        }
        blocker.block();
        int bad = 0;
        int count = 0;
        for (Node n = head; n != null; n = n.next) {
            long want = seed * 1_000_003L + (23 - count);
            if (n.value != want
                    || n.payload.length != 3
                    || n.payload[0] != (int) want
                    || n.payload[1] != (int) (want >>> 32)
                    || n.payload[2] != (int) (want * 31)) {
                bad++;
            }
            count++;
        }
        if (count != 24) {
            bad++;
        }
        if (!label.equals("r" + seed)) {
            bad++;
        }
        for (int i = 0; i < extra.length; i++) {
            if (extra[i] != (seed ^ (i * 0x9E3779B97F4A7C15L))) {
                bad++;
            }
        }
        return bad;
    }

    public static void main(String[] a) throws Exception {
        int rounds = Integer.getInteger("nio21.rounds", 600);
        int workers = Integer.getInteger("nio21.workers", 4);
        System.out.println(runPipe(rounds, workers));
        System.out.println(runQueue(rounds, workers));
    }

    static String runPipe(int rounds, int workers) throws Exception {
        Pipe[] pipes = new Pipe[workers];
        for (int w = 0; w < workers; w++) {
            pipes[w] = Pipe.open();
        }
        int[] bad = new int[workers];
        Thread[] ts = new Thread[workers];
        for (int w = 0; w < workers; w++) {
            final int id = w;
            ts[w] = new Thread(() -> {
                ByteBuffer one = ByteBuffer.allocate(1);
                Blocker b = () -> {
                    one.clear();
                    while (one.hasRemaining()) {
                        pipes[id].source().read(one);
                    }
                };
                try {
                    for (int r = 0; r < rounds; r++) {
                        bad[id] += round(b, (long) id * rounds + r);
                    }
                } catch (Exception e) {
                    bad[id] += 1_000_000;
                }
            }, "pipe-worker-" + w);
        }
        return drive("pipe", rounds, workers, ts, bad, (w) -> {
            ByteBuffer one = ByteBuffer.wrap(new byte[] {1});
            while (one.hasRemaining()) {
                pipes[w].sink().write(one);
            }
        });
    }

    static String runQueue(int rounds, int workers) throws Exception {
        @SuppressWarnings("unchecked")
        BlockingQueue<Integer>[] queues = new BlockingQueue[workers];
        for (int w = 0; w < workers; w++) {
            queues[w] = new ArrayBlockingQueue<>(4);
        }
        int[] bad = new int[workers];
        Thread[] ts = new Thread[workers];
        for (int w = 0; w < workers; w++) {
            final int id = w;
            ts[w] = new Thread(() -> {
                Blocker b = () -> queues[id].take();
                try {
                    for (int r = 0; r < rounds; r++) {
                        bad[id] += round(b, (long) id * rounds + r + 7);
                    }
                } catch (Exception e) {
                    bad[id] += 1_000_000;
                }
            }, "queue-worker-" + w);
        }
        return drive("queue", rounds, workers, ts, bad, (w) -> queues[w].put(w));
    }

    interface Release {
        void release(int worker) throws Exception;
    }

    static String drive(String row, int rounds, int workers, Thread[] ts, int[] bad, Release release)
            throws Exception {
        AtomicBoolean stop = new AtomicBoolean();
        Thread churn = new Thread(() -> {
            int n = 0;
            while (!stop.get()) {
                Object[] junk = new Object[64];
                for (int i = 0; i < junk.length; i++) {
                    junk[i] = new byte[128 + (i & 15) * 16];
                }
                sink = junk;
                if ((++n & 255) == 0) {
                    System.gc();
                }
            }
        }, "churn");
        churn.setDaemon(true);
        churn.start();
        for (Thread t : ts) {
            t.start();
        }
        for (int r = 0; r < rounds; r++) {
            for (int w = 0; w < workers; w++) {
                release.release(w);
            }
            if ((r & 31) == 0) {
                System.gc();
            }
        }
        for (Thread t : ts) {
            t.join();
        }
        stop.set(true);
        churn.join();
        int total = 0;
        for (int b : bad) {
            total += b;
        }
        return "blocked-roots " + row + " rounds=" + rounds + " workers=" + workers + " bad=" + total;
    }
}
