import java.io.*; import java.nio.*; import java.nio.channels.*; import java.nio.file.*;
/** Map/unmap a file repeatedly — no Lucene, no Runtime.Version involved. */
public class MapLeak {
  public static void main(String[] a) throws Exception {
    int n = Integer.parseInt(a[0]);
    Path p = Path.of("maptarget.bin");
    byte[] buf = new byte[8 * 1024 * 1024];
    Files.write(p, buf);
    long sum = 0;
    for (int i = 0; i < n; i++) {
      try (FileChannel ch = FileChannel.open(p, StandardOpenOption.READ)) {
        MappedByteBuffer m = ch.map(FileChannel.MapMode.READ_ONLY, 0, ch.size());
        sum += m.get(0) + m.get((int) ch.size() - 1);
      }
      if (i % 20 == 0) System.out.println("i=" + i + " rss-check");
    }
    System.out.println("sum=" + sum);
  }
}
