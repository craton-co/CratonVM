import java.util.Enumeration;
import java.net.URL;
public class GetResourcesRepro {
  static void check(String tag, Enumeration<URL> e) {
    System.out.println(tag + " -> " + (e == null ? "NULL!!" : "ok(empty=" + !e.hasMoreElements() + ")"));
  }
  public static void main(String[] a) throws Exception {
    ClassLoader cl = GetResourcesRepro.class.getClassLoader();
    check("cl.getResources(arquillian-LoadableExtension)", cl.getResources("META-INF/services/org.jboss.arquillian.core.spi.LoadableExtension"));
    check("cl.getResources(nonexistent)", cl.getResources("no/such/resource.xyz"));
    check("cl.getResources(MANIFEST)", cl.getResources("META-INF/MANIFEST.MF"));
    check("TCCL.getResources(x)", Thread.currentThread().getContextClassLoader().getResources("META-INF/services/x"));
    check("ClassLoader.getSystemResources(x)", ClassLoader.getSystemResources("META-INF/services/x"));
    System.out.println("done");
  }
}
