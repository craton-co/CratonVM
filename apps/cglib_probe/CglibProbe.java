import net.sf.cglib.proxy.Enhancer;
import net.sf.cglib.proxy.MethodInterceptor;

/** Standalone (unshaded) CGLIB regression witness for CGL.1. */
public class CglibProbe {
    public static class Greeter {
        public String hello(String name) { return "hello " + name; }
    }

    public static void main(String[] args) {
        Enhancer enhancer = new Enhancer();
        enhancer.setSuperclass(Greeter.class);
        enhancer.setCallback((MethodInterceptor) (object, method, arguments, proxy) ->
                proxy.invokeSuper(object, arguments) + " (cglib)");
        Greeter greeter = (Greeter) enhancer.create();
        String result = greeter.hello("world");
        System.out.println("proxy.out=" + result);
        if (!"hello world (cglib)".equals(result)) {
            throw new AssertionError("interceptor result=" + result);
        }
        System.out.println("CglibProbe: PASS");
    }
}