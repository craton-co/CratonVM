// Concurrent positional FileChannel reads/writes -- the census's "buf is null" crash through the real Util bytecode.
//
// Record: nonpassed-classbyclass-census-RESOLVED-20260923.md (D2)
// Run: cratonvm --java-home <jdk-25> -c <dir>[:<classpath>] BufRaceProbe <threads> <iterations>
// Compare with the same command on HotSpot (java -cp ...).
import java.io.*;
import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.atomic.*;

public class BufRaceProbe {
    public static void main(String[] a) throws Exception {
        int threads = a.length > 0 ? Integer.parseInt(a[0]) : 8;
        int iters = a.length > 1 ? Integer.parseInt(a[1]) : 5000;
        Path dir = Files.createTempDirectory("bufrace");
        AtomicInteger fails = new AtomicInteger();
        AtomicReference<Throwable> first = new AtomicReference<>();
        List<Thread> ts = new ArrayList<>();
        long t0 = System.nanoTime();
        for (int t = 0; t < threads; t++) {
            final int id = t;
            Path f = dir.resolve("f" + t);
            byte[] data = new byte[256 * 1024];
            new Random(t).nextBytes(data);
            Files.write(f, data);
            Thread th = new Thread(() -> {
                Random r = new Random(id * 31L);
                try (FileChannel ch = FileChannel.open(f, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
                    for (int i = 0; i < iters; i++) {
                        int size = 1 + r.nextInt(64 * 1024);
                        ByteBuffer bb = ByteBuffer.allocate(size);
                        long pos = r.nextInt(data.length - size);
                        try {
                            if ((i & 3) == 3) {
                                ch.write(ByteBuffer.wrap(data, (int) pos, size), pos);
                            } else {
                                int n = ch.read(bb, pos);
                                if (n > 0 && bb.get(0) != data[(int) pos]) throw new IllegalStateException("data mismatch at " + pos);
                            }
                        } catch (Throwable e) {
                            fails.incrementAndGet();
                            first.compareAndSet(null, e);
                        }
                    }
                } catch (IOException e) {
                    first.compareAndSet(null, e);
                }
            }, "w" + t);
            ts.add(th);
        }
        for (Thread th : ts) th.start();
        for (Thread th : ts) th.join();
        System.out.println("threads=" + threads + " iters=" + iters + " fails=" + fails.get() + " ms=" + (System.nanoTime() - t0) / 1_000_000);
        if (first.get() != null) first.get().printStackTrace(System.out);
    }
}
