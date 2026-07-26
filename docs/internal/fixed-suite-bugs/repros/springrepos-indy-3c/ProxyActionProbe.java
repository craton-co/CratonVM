import java.lang.reflect.*;
import org.gradle.api.Action;
public class ProxyActionProbe {
  public static void main(String[] x) throws Throwable {
    // 1. Plain JDK dynamic proxy of Action with a lambda InvocationHandler.
    InvocationHandler h = (proxy, method, args) -> {
      System.out.println("  handler invoked: " + method.getName() + " argc=" + (args==null?0:args.length));
      return null;
    };
    Object p = Proxy.newProxyInstance(ProxyActionProbe.class.getClassLoader(),
        new Class<?>[]{ Action.class }, h);
    System.out.println("proxy class=" + p.getClass().getName() + " isAction=" + (p instanceof Action));
    Action a = (Action) p;
    System.out.println("calling a.execute(\"X\") ...");
    a.execute("X");
    System.out.println("PROXYACTIONPROBE_OK");
  }
}
