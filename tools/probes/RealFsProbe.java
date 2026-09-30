// The real jdk.internal.jrtfs under --jdk-only: installed providers, /modules listing, a class read, a module walk, JrtPath.toString.
//
// Record: fs-cluster-needtoresolveagainstdefaultdirectory-FIXED-20260923.md
// Run: cratonvm --java-home <jdk-25> -c <dir>[:<classpath>] RealFsProbe
// Compare with the same command on HotSpot (java -cp ...).
import java.net.URI;
import java.nio.file.*;
import java.nio.file.spi.FileSystemProvider;
import java.util.*;

public class RealFsProbe {
    public static void main(String[] a) throws Exception {
        List<FileSystemProvider> real = new ArrayList<>();
        for (FileSystemProvider p : ServiceLoader.load(FileSystemProvider.class, ClassLoader.getSystemClassLoader())) {
            System.out.println("service provider: " + p.getClass().getName() + " scheme=" + p.getScheme());
            real.add(p);
        }
        for (FileSystemProvider p : FileSystemProvider.installedProviders()) {
            System.out.println("installed provider: " + p.getClass().getName() + " scheme=" + p.getScheme());
        }
        FileSystemProvider jrtp = null;
        for (FileSystemProvider p : real) if (p.getScheme().equals("jrt")) jrtp = p;
        if (jrtp == null) {
            Class<?> c = Class.forName("jdk.internal.jrtfs.JrtFileSystemProvider");
            var ctor = c.getDeclaredConstructor();
            ctor.setAccessible(true);
            jrtp = (FileSystemProvider) ctor.newInstance();
            System.out.println("constructed directly: " + jrtp.getClass().getName());
        }
        FileSystem jrt = a.length > 0 && a[0].equals("home")
                ? jrtp.newFileSystem(URI.create("jrt:/"), Map.of("java.home", System.getProperty("java.home")))
                : jrtp.getFileSystem(URI.create("jrt:/"));
        System.out.println("real jrt fs=" + jrt.getClass().getName());
        Path modules = jrt.getPath("/modules");
        int n = 0;
        try (DirectoryStream<Path> ds = Files.newDirectoryStream(modules, Files::isDirectory)) {
            for (Path p : ds) { if (n < 2) System.out.println("  " + p.getClass().getName() + " " + p); n++; }
        }
        System.out.println("modules entries=" + n);
        Path obj = jrt.getPath("/modules/java.base/java/lang/Object.class");
        byte[] b = Files.readAllBytes(obj);
        System.out.println("Object.class bytes=" + b.length + " magic=" + Integer.toHexString(((b[0]&0xff)<<24)|((b[1]&0xff)<<16)|((b[2]&0xff)<<8)|(b[3]&0xff)));
        Path pkgs = jrt.getPath("/packages/java.lang");
        try (DirectoryStream<Path> ds = Files.newDirectoryStream(pkgs)) {
            for (Path p : ds) System.out.println("  pkg link " + p + " isLink=" + Files.isSymbolicLink(p) + " real=" + p.toRealPath());
        }
        long cnt;
        try (var s = Files.walk(jrt.getPath("/modules/java.sql"))) { cnt = s.count(); }
        System.out.println("java.sql walk count=" + cnt);
        // zip provider
        FileSystemProvider zipp = null;
        for (FileSystemProvider p : real) if (p.getScheme().equals("jar")) zipp = p;
        System.out.println("zip provider=" + (zipp == null ? null : zipp.getClass().getName()));
    }
}
