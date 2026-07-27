import org.h2.store.fs.FileUtils;

import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.util.Random;
import java.util.concurrent.atomic.AtomicIntegerArray;

/**
 * Isolated replica of H2 TestFileSystem.testConcurrent, with per-phase timing,
 * so residual 4 can be measured in seconds instead of a 25-minute cap.
 *
 *   java LzfProbe <fsPrefix> <operations> [threads]
 *
 * Phases timed separately so a flat interpreter profile can be attributed:
 *   OPEN  - createTempFile + open + the initial size*64K write
 *   OPS   - the writer loop (write, read-back, assert), in blocks of 100
 * Set -Dprobe.reader=false to run the writer alone (no spin-lock contention),
 * which separates "the filesystem is slow" from "the spin lock is slow".
 */
public class LzfProbe {

    static volatile boolean stop = false;
    static volatile String failure = null;

    public static void main(String[] args) throws Exception {
        String fsBase = args.length > 0 ? args[0] : "nioMemLZF:1:/probe";
        int operations = args.length > 1 ? Integer.parseInt(args[1]) : 300;
        boolean withReader = !"false".equals(System.getProperty("probe.reader"));
        int size = 10;

        long t0 = System.nanoTime();
        String s = FileUtils.createTempFile(fsBase + "/tmp", ".tmp", false);
        FileUtils.delete(s);
        final FileChannel f = FileUtils.open(s, "rw");
        f.write(ByteBuffer.allocate(size * 64 * 1024));
        System.out.printf("OPEN %s  %.1f ms%n", fsBase, (System.nanoTime() - t0) / 1e6);

        final AtomicIntegerArray locks = new AtomicIntegerArray(size);
        final AtomicIntegerArray expected = new AtomicIntegerArray(size);
        final int fsize = size;
        Random random = new Random(1);

        Thread reader = null;
        if (withReader) {
            reader = new Thread(() -> {
                ByteBuffer bb = ByteBuffer.allocate(16);
                try {
                    while (!stop) {
                        for (int pos = 0; pos < fsize; pos++) {
                            bb.clear();
                            int e;
                            while (!locks.compareAndSet(pos, 0, 1)) {
                                // backoff-free, exactly as H2 spells it
                            }
                            try {
                                e = expected.get(pos);
                                f.read(bb, pos * 64 * 1024);
                            } finally {
                                locks.set(pos, 0);
                            }
                            bb.position(0);
                            int x = bb.getInt();
                            int y = bb.getInt();
                            if (e != x || e != y) {
                                failure = "reader: expected " + e + " got " + x + "/" + y;
                                return;
                            }
                            Thread.yield();
                        }
                    }
                } catch (Throwable t) {
                    failure = "reader threw " + t;
                }
            });
            reader.start();
        }

        ByteBuffer bb = ByteBuffer.allocate(16);
        long block = System.nanoTime();
        long start = block;
        for (int i = 0; i < operations; i++) {
            bb.position(0);
            bb.putInt(i);
            bb.putInt(i);
            bb.flip();
            int pos = random.nextInt(size);
            while (!locks.compareAndSet(pos, 0, 1)) {
                // spin
            }
            try {
                f.write(bb, pos * 64 * 1024);
                expected.set(pos, i);
            } finally {
                locks.set(pos, 0);
            }
            pos = random.nextInt(size);
            bb.clear();
            int e;
            while (!locks.compareAndSet(pos, 0, 1)) {
                // spin
            }
            try {
                e = expected.get(pos);
                f.read(bb, pos * 64 * 1024);
            } finally {
                locks.set(pos, 0);
            }
            bb.limit(16);
            bb.position(0);
            int x = bb.getInt();
            int y = bb.getInt();
            if (e != x || e != y) {
                failure = "writer: expected " + e + " got " + x + "/" + y;
                break;
            }
            if ((i + 1) % 100 == 0) {
                long now = System.nanoTime();
                System.out.printf("OPS %5d  %8.1f ms/100%n", i + 1, (now - block) / 1e6);
                block = now;
            }
            if (failure != null) {
                break;
            }
        }
        long total = System.nanoTime() - start;
        stop = true;
        if (reader != null) {
            reader.join(20000);
        }
        f.close();
        FileUtils.delete(s);
        System.out.printf("TOTAL %d ops in %.1f ms  (%.2f ms/op)%n",
                operations, total / 1e6, total / 1e6 / operations);
        if (failure != null) {
            System.out.println("FAILURE " + failure);
            System.exit(1);
        }
        System.out.println("OK " + fsBase);
    }
}
