// Descriptors leaked by FileChannel.open/close, map, Files.newInputStream/newOutputStream, RandomAccessFile (reads /proc/self/fd).
//
// Record: nonpassed-classbyclass-census-RESOLVED-20260923.md (D3)
// Run: cratonvm --java-home <jdk-25> -c <dir>[:<classpath>] FdLeakProbe
// Compare with the same command on HotSpot (java -cp ...).
import java.io.*;
import java.nio.*;
import java.nio.channels.*;
import java.nio.file.*;

public class FdLeakProbe {
    static long fds() { String[] l = new File("/proc/self/fd").list(); return l == null ? -1 : l.length; }
    public static void main(String[] a) throws Exception {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 300;
        Path f = Files.createTempFile("fdleak", ".bin");
        Files.write(f, new byte[64 * 1024]);
        long base = fds();
        for (int i = 0; i < n; i++) { try (FileChannel c = FileChannel.open(f, StandardOpenOption.READ)) { c.size(); } }
        System.out.println("open/close        leaked=" + (fds() - base));
        base = fds();
        for (int i = 0; i < n; i++) { try (FileChannel c = FileChannel.open(f, StandardOpenOption.READ)) { MappedByteBuffer m = c.map(FileChannel.MapMode.READ_ONLY, 0, c.size()); m.get(0); } }
        System.out.println("open/map/close    leaked=" + (fds() - base));
        base = fds();
        for (int i = 0; i < n; i++) { try (RandomAccessFile r = new RandomAccessFile(f.toFile(), "r")) { r.read(); } }
        System.out.println("RandomAccessFile  leaked=" + (fds() - base));
        base = fds();
        for (int i = 0; i < n; i++) { try (InputStream in = Files.newInputStream(f)) { in.read(); } }
        System.out.println("Files.newInputStream leaked=" + (fds() - base));
        base = fds();
        for (int i = 0; i < n; i++) { try (OutputStream o = Files.newOutputStream(f.resolveSibling("fdleak-out-" + (i % 3)))) { o.write(1); } }
        System.out.println("Files.newOutputStream leaked=" + (fds() - base));
        base = fds();
        for (int i = 0; i < n; i++) { try (FileChannel c = FileChannel.open(f, StandardOpenOption.READ, StandardOpenOption.WRITE)) { c.write(ByteBuffer.wrap(new byte[]{1}), 0); c.force(true); } }
        System.out.println("open rw/force/close leaked=" + (fds() - base));
        base = fds();
        for (int i = 0; i < n; i++) { try (FileChannel c = FileChannel.open(f.getParent(), StandardOpenOption.READ)) { c.force(true); } catch (IOException e) { if (i == 0) System.out.println("dir fsync: " + e); } }
        System.out.println("dir open/force/close leaked=" + (fds() - base));
        base = fds();
        for (int i = 0; i < n; i++) { try (DirectoryStream<Path> d = Files.newDirectoryStream(f.getParent(), "fdleak*")) { for (Path p : d) {} } }
        System.out.println("newDirectoryStream leaked=" + (fds() - base));
    }
}
