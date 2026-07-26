import groovy.lang.GroovyClassLoader;
import org.gradle.api.Action;
import java.lang.reflect.Proxy;
import java.util.Arrays;
public class SamRealProbe {
  public interface Sink { void mavenContent(Action<String> a); }
  public static class RealSink implements Sink {
    public int fired=0;
    public void mavenContent(Action<String> a) {
      fired++;
      System.out.println("  RealSink.mavenContent(Action) called; arg class=" + a.getClass().getName());
      System.out.println("  arg isAction=" + (a instanceof Action) + " isProxy=" + Proxy.isProxyClass(a.getClass()));
      System.out.println("  arg interfaces=" + Arrays.toString(a.getClass().getInterfaces()));
      try { a.execute("X"); System.out.println("  execute OK"); }
      catch (Throwable t) { System.out.println("  execute THREW: " + t); }
    }
  }
  public static void main(String[] x) throws Throwable {
    GroovyClassLoader gcl = new GroovyClassLoader(SamRealProbe.class.getClassLoader());
    String src = "class S2 { def go(repo, holder) { repo.mavenContent { mc -> holder[0]++ ; println('    closure ran with '+mc) } } }\n";
    Class<?> c = gcl.parseClass(src);
    Object s = c.getDeclaredConstructor().newInstance();
    RealSink sink = new RealSink();
    int[] holder = {0};
    try { c.getDeclaredMethod("go", Object.class, Object.class).invoke(s, sink, holder); }
    catch (Throwable t) { System.out.println("  go THREW: " + t.getCause()); }
    System.out.println("RealSink fired=" + sink.fired + " closureRan=" + holder[0]);
    System.out.println("SAMREALPROBE_DONE");
  }
}
