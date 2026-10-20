import java.net.URI;
import java.nio.file.*;

/** SBR5 warmup lane: first use of the jrt: file system (reads lib/modules through jdk.internal.jimage). */
public class Sbr5WarmupJrt {
    public static void main(String[] a) throws Exception {
        long t0 = System.nanoTime();
        FileSystem fs = FileSystems.getFileSystem(URI.create("jrt:/"));
        long t1 = System.nanoTime();
        Path p = fs.getPath("/modules/java.base/java/lang/Object.class");
        long size = Files.size(p);
        long t2 = System.nanoTime();
        byte[] b = Files.readAllBytes(p);
        long t3 = System.nanoTime();
        System.out.println("size=" + size + " read=" + b.length + " open=" + (t1 - t0) / 1_000_000 + "ms stat=" + (t2 - t1) / 1_000_000 + "ms read=" + (t3 - t2) / 1_000_000 + "ms");
    }
}
