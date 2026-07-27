import java.beans.Introspector;
import java.io.InputStream;
public class ModProbe {
  public static void main(String[] a) throws Exception {
    Module m = Introspector.class.getModule();
    System.out.println("module = " + m + " named=" + m.isNamed() + " name=" + m.getName());
    String p = "java/beans/Introspector.class";
    try (InputStream in = m.getResourceAsStream(p)) {
      System.out.println("getResourceAsStream(" + p + ") = " + in);
      if (in != null) System.out.println("  bytes = " + in.readAllBytes().length);
    }
    try (InputStream in = m.getResourceAsStream("java/beans/DoesNotExist.class")) {
      System.out.println("missing -> " + in);
    }
  }
}
