import java.util.jar.JarFile;
import java.util.zip.ZipFile;
import java.util.Enumeration;
public class JarEntriesRepro {
  public static void main(String[] a) throws Exception {
    JarFile jf = new JarFile(a[0]);
    Enumeration<?> e = jf.entries();
    System.out.println("JarFile.entries() -> " + (e == null ? "NULL!!" : "ok"));
    if (e != null) { int n=0; while(e.hasMoreElements()){e.nextElement();n++;} System.out.println("  jar count=" + n); }
    ZipFile zf = new ZipFile(a[0]);
    Enumeration<?> z = zf.entries();
    System.out.println("ZipFile.entries() -> " + (z == null ? "NULL!!" : "ok"));
  }
}
