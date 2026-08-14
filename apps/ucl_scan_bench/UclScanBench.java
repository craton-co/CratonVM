import java.io.File;
import java.net.URL;
import java.net.URLClassLoader;
import java.util.ArrayList;
import java.util.List;

/**
 * Times repeated Class.forName hits/misses on a URLClassLoader built over a
 * large (~121 jar) classpath -- the shape Spring Boot's ClassUtils.isPresent()
 * drives hundreds of times per ApplicationContext refresh.
 *
 * Args: <classpath-file> <iterations>
 */
public class UclScanBench {
  public static void main(String[] args) throws Exception {
    String cpFile = args[0];
    int iters = args.length > 1 ? Integer.parseInt(args[1]) : 100;

    String cp = new String(java.nio.file.Files.readAllBytes(java.nio.file.Paths.get(cpFile))).trim();
    List<URL> urls = new ArrayList<>();
    for (String e : cp.split(File.pathSeparator)) {
      if (e.isEmpty()) continue;
      File f = new File(e);
      if (f.exists()) urls.add(f.toURI().toURL());
    }
    System.out.println("classpath entries: " + urls.size());

    URLClassLoader ucl = new URLClassLoader(urls.toArray(new URL[0]), null);

    // Names that exist on the classpath (hits) and names that do not (misses).
    String[] hits = {
      "org.springframework.util.ClassUtils",
      "org.springframework.core.io.Resource",
      "org.springframework.beans.factory.BeanFactory",
      "org.springframework.context.ApplicationContext",
    };
    String[] misses = {
      "com.example.absent.Alpha",
      "com.example.absent.Beta",
      "com.example.absent.Gamma",
      "com.example.absent.Delta",
    };

    long t0 = System.nanoTime();
    int found = 0, missed = 0;
    for (int i = 0; i < iters; i++) {
      for (String h : hits) {
        try { Class.forName(h, false, ucl); found++; } catch (Throwable t) { }
      }
      for (String m : misses) {
        try { Class.forName(m, false, ucl); } catch (Throwable t) { missed++; }
      }
    }
    long ms = (System.nanoTime() - t0) / 1_000_000L;
    System.out.println("iters=" + iters + " found=" + found + " missed=" + missed);
    System.out.println("BENCH_TOTAL_MS=" + ms);
  }
}
