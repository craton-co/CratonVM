// Files.isDirectory on a jrt: path -- the NoSuchMethodError (needToResolveAgainstDefaultDirectory) on binaries before 9c7b4ed2e.
//
// Record: fs-cluster-needtoresolveagainstdefaultdirectory-FIXED-20260923.md
// Run: cratonvm --java-home <jdk-25> -c <dir>[:<classpath>] JrtProbe [iterations]
// Compare with the same command on HotSpot (java -cp ...).
import java.net.URI;
import java.nio.file.*;
import java.util.*;

public class JrtProbe {
    public static void main(String[] a) throws Exception {
        int iters = a.length > 0 ? Integer.parseInt(a[0]) : 200;
        int warm = a.length > 1 ? Integer.parseInt(a[1]) : 0;
        Path tmp = Paths.get(System.getProperty("java.io.tmpdir"));
        int dirs = 0;
        for (int i = 0; i < warm; i++) { if (Files.isDirectory(tmp)) dirs++; if (Files.exists(tmp.resolve("nope" + (i & 7)))) dirs--; }
        System.out.println("warm=" + warm + " dirs=" + dirs);
        FileSystem def = FileSystems.getDefault();
        System.out.println("default fs class=" + def.getClass().getName() + " provider=" + def.provider().getClass().getName());
        FileSystem jrt = FileSystems.getFileSystem(URI.create("jrt:/"));
        System.out.println("jrt fs class=" + jrt.getClass().getName() + " provider=" + jrt.provider().getClass().getName()
                + " scheme=" + jrt.provider().getScheme());
        Path modules = jrt.getPath("/modules");
        System.out.println("modules path class=" + modules.getClass().getName() + " str=" + modules + " fsSame=" + (modules.getFileSystem() == jrt));
        int fails = 0;
        long total = 0;
        String firstErr = null;
        for (int i = 0; i < iters; i++) {
            try (DirectoryStream<Path> ds = Files.newDirectoryStream(modules, Files::isDirectory)) {
                int n = 0;
                for (Path p : ds) {
                    if (i == 0 && n < 2) System.out.println("  entry class=" + p.getClass().getName() + " name=" + p.getFileName()
                            + " fsclass=" + p.getFileSystem().getClass().getName());
                    n++;
                }
                total += n;
            } catch (Throwable t) {
                fails++;
                if (firstErr == null) { firstErr = "iter " + i + ": " + t; t.printStackTrace(System.out); }
            }
        }
        System.out.println("iters=" + iters + " fails=" + fails + " avgEntries=" + (total / Math.max(1, iters - fails)) + " firstErr=" + firstErr);
        Path jbase = modules.resolve("java.base");
        System.out.println("java.base isDir=" + Files.isDirectory(jbase) + " Object.class exists=" + Files.exists(jbase.resolve("java/lang/Object.class"))
                + " size=" + Files.size(jbase.resolve("java/lang/Object.class")));
    }
}
