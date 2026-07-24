import java.lang.reflect.*;
import groovy.lang.Closure;
import groovy.lang.GroovyClassLoader;
import org.codehaus.groovy.runtime.ConvertedClosure;
import org.gradle.api.Action;
public class ConvClosureProbe {
  public static void main(String[] x) throws Throwable {
    GroovyClassLoader gcl = new GroovyClassLoader(ConvClosureProbe.class.getClassLoader());
    Class<?> c = gcl.parseClass("class K { static Closure mk(holder){ return { v -> holder[0] = ('got:'+v) } } }");
    int[] holder = {0}; // will be replaced
    Object[] hb = new Object[1];
    Closure<?> closure = (Closure<?>) c.getDeclaredMethod("mk", Object.class).invoke(null, (Object) hb);

    InvocationHandler handler = new ConvertedClosure(closure, "execute");
    Object p = Proxy.newProxyInstance(Action.class.getClassLoader(),
        new Class<?>[]{ Action.class }, handler);
    System.out.println("proxy=" + p.getClass().getName() + " isAction=" + (p instanceof Action));
    Action a = (Action) p;
    a.execute("HELLO");
    System.out.println("closure side effect=" + hb[0] + " (expect got:HELLO)");
    System.out.println("CONVCLOSUREPROBE_OK");
  }
}
