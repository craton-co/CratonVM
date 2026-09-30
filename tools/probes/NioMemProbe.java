// TestFileSystem.testConcurrent's op mix, single-threaded, per H2 filesystem prefix. Classpath: <h2 target/classes>.
//
// Record: docs/known-issues/h2/h2-throughput-and-budget-residuals-20260923.md
// Run: cratonvm --java-home <jdk-25> -c <dir>[:<classpath>] NioMemProbe <fs-prefix>...
// Compare with the same command on HotSpot (java -cp ...).
import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.util.Random;

public class NioMemProbe {
    public static void main(String[] a) throws Exception {
        for (String fs : a) {
            long t0 = System.nanoTime();
            FileChannel f = org.h2.store.fs.FileUtils.open(fs + "probe" + System.nanoTime(), "rw");
            int size = 10;
            f.write(ByteBuffer.allocate(size * 64 * 1024));
            Random random = new Random(1);
            ByteBuffer b = ByteBuffer.allocate(16);
            for (int i = 0; i < 10000; i++) {
                b.clear(); b.putInt(i); b.putInt(i); b.flip();
                f.write(b, random.nextInt(size) * 64 * 1024);
                b.clear();
                f.read(b, random.nextInt(size) * 64 * 1024);
            }
            f.close();
            System.out.printf("%-16s 10000 ops: %d ms%n", fs, (System.nanoTime() - t0) / 1000000);
        }
    }
}
