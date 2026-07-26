import groovy.lang.GroovyClassLoader;

// Stress ANTLR ParserATNSimulator.closure() recursion depth with deeply-nested
// method-call-with-closure expressions, to test whether deep JIT'd closure
// recursion stack-overflows (the SIGSEGV seen with the cold-path JIT change).
public class GroovyNestProbe {
  static String nested(int depth) {
    StringBuilder sb = new StringBuilder("class D { def run() {\n  a");
    for (int i = 0; i < depth; i++) sb.append(" { b");
    sb.append(" { c() }");
    for (int i = 0; i < depth; i++) sb.append(" }");
    sb.append("\n} }\n");
    return sb.toString();
  }
  public static void main(String[] a) {
    GroovyClassLoader gcl = new GroovyClassLoader();
    long w = System.currentTimeMillis();
    gcl.parseClass("class Warm { int z(int x){ return x+1 } }", "Warm.groovy");
    System.out.println("WARMUP " + (System.currentTimeMillis()-w) + "ms");
    int[] depths = {20, 40, 80, 160, 320, 640};
    for (int d : depths) {
      String src = nested(d);
      long t = System.currentTimeMillis();
      try {
        gcl.parseClass(src, "D" + d + ".groovy");
        System.out.println("depth=" + d + " parsed " + (System.currentTimeMillis()-t) + "ms");
        System.out.flush();
      } catch (Throwable e) {
        System.out.println("depth=" + d + " THREW " + e.getClass().getSimpleName() + ": " + e.getMessage());
        System.out.flush();
      }
    }
    System.out.println("DONE-ALL");
  }
}
