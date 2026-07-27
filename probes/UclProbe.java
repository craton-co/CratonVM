import java.net.*;
import java.nio.file.*;
import java.util.*;

/** Directory on disk is literally "custom#root"; the URL spells it "custom%23root",
 *  so URLClassLoader must percent-DECODE before touching the filesystem. */
public class UclProbe {
  public static void main(String[] a) throws Exception {
    Path base = Files.createTempDirectory("ucl");
    Path root = base.resolve("custom#root");
    Path scanned = root.resolve("scanned");
    Files.createDirectories(scanned);
    Files.writeString(scanned.resolve("resource1.txt"), "x");
    URL rootUrl = new URL("file:" + base.toAbsolutePath() + "/custom%23root/");
    System.out.println("rootUrl = " + rootUrl);
    try (URLClassLoader cl = new URLClassLoader(new URL[]{rootUrl})) {
      System.out.println("getResource(scanned/resource1.txt) = " + cl.getResource("scanned/resource1.txt"));
      Enumeration<URL> e = cl.getResources("scanned/");
      int n = 0;
      while (e.hasMoreElements()) { System.out.println("  res: " + e.nextElement()); n++; }
      System.out.println("getResources(scanned/) count = " + n);
    }
  }
}
