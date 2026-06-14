import java.util.regex.Pattern;
public class RegexBench2 {
  public static void main(String[] a) {
    String cls = "org.junit.jupiter.engine.descriptor.ClassBasedTestDescriptor";
    Pattern p = Pattern.compile("[.]");
    // warmup (let JIT engage)
    for (int i = 0; i < 20000; i++) p.matcher(cls).replaceAll("/");
    long t0 = System.nanoTime();
    String s = null;
    for (int i = 0; i < 20000; i++) s = p.matcher(cls).replaceAll("/");
    long t1 = System.nanoTime();
    System.out.println("precompiled x20000 (warm) = " + (t1-t0)/1_000_000 + "ms -> " + s);
  }
}
