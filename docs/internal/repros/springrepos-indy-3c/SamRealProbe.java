import groovy.lang.GroovyClassLoader;
import org.gradle.api.Action;
public class SamRealProbe {
  public interface Sink { void mavenContent(Action<String> a); }
  public static class RealSink implements Sink {
    public int fired=0, executed=0;
    public void mavenContent(Action<String> a) { fired++; System.out.println("  RealSink.mavenContent(Action) called"); a.execute("X"); }
  }
  public static void main(String[] x) throws Throwable {
    GroovyClassLoader gcl = new GroovyClassLoader(SamRealProbe.class.getClassLoader());
    String src = "class S2 { def go(repo, holder) { repo.mavenContent { mc -> holder[0]++ ; println('    closure ran') } } }\n";
    Class<?> c = gcl.parseClass(src);
    Object s = c.getDeclaredConstructor().newInstance();
    RealSink sink = new RealSink();
    int[] holder = {0};
    try {
      c.getDeclaredMethod("go", Object.class, Object.class).invoke(s, sink, holder);
    } catch (Throwable t) { System.out.println("  THREW: " + t.getCause()); }
    System.out.println("RealSink fired=" + sink.fired + " closureRan=" + holder[0] + " (expect 1 1)");
    System.out.println("SAMREALPROBE_DONE");
  }
}
