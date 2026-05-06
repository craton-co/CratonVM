import net.sf.cglib.proxy.Enhancer;
import net.sf.cglib.proxy.MethodInterceptor;
import net.sf.cglib.proxy.MethodProxy;
import java.lang.reflect.Method;

public class CglibProbe {
    public static class Greeter {
        public String hello(String name) { return "hello " + name; }
    }

    public static void main(String[] args) {
        Enhancer e = new Enhancer();
        e.setSuperclass(Greeter.class);
        e.setCallback((MethodInterceptor) (obj, m, ar, p) -> {
            Object r = p.invokeSuper(obj, ar);
            return r + " (cglib)";
        });
        Greeter proxy = (Greeter) e.create();
        String out = proxy.hello("world");
        System.out.println("proxy.out=" + out);
        if (!out.contains("(cglib)")) throw new AssertionError("interceptor missed");
        System.out.println("CglibProbe: PASS");
    }
}
