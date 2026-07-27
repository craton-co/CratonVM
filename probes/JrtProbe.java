import java.net.*;
public class JrtProbe {
  public static void main(String[] a) throws Exception {
    URL u = JrtProbe.class.getClassLoader().getResource("java/beans/Introspector.class");
    if (u == null) u = ClassLoader.getSystemResource("java/beans/Introspector.class");
    System.out.println("url = " + u);
    if (u == null) { System.out.println("no url"); return; }
    URLConnection c = u.openConnection();
    System.out.println("conn = " + c.getClass().getName());
    System.out.println("getContentLengthLong = " + c.getContentLengthLong());
    System.out.println("getContentLength     = " + c.getContentLength());
    try (var in = u.openStream()) { System.out.println("stream bytes = " + in.readAllBytes().length); }
  }
}
