import java.lang.reflect.*;
public class ProxyProbe {
  public static void main(String[] a) {
    Object p = Proxy.newProxyInstance(
        ProxyProbe.class.getClassLoader(),
        new Class[]{Runnable.class},
        (proxy, m, args) -> { System.out.println("invoked " + m.getName()); return null; });
    ((Runnable)p).run();
    System.out.println("PROXY_OK");
  }
}
