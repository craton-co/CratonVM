import java.util.regex.Pattern;
public class RegexBench {
  public static void main(String[] a) {
    int N = 2000;
    String cls = "org.junit.jupiter.engine.descriptor.ClassBasedTestDescriptor";
    long t0 = System.nanoTime();
    String s = null;
    for (int i = 0; i < N; i++) s = cls.replaceAll("[.]", "/");
    long t1 = System.nanoTime();
    System.out.println("String.replaceAll x" + N + " = " + (t1-t0)/1_000_000 + "ms -> " + s);
    Pattern p = Pattern.compile("[.]");
    long t2 = System.nanoTime();
    for (int i = 0; i < N; i++) s = p.matcher(cls).replaceAll("/");
    long t3 = System.nanoTime();
    System.out.println("precompiled x" + N + " = " + (t3-t2)/1_000_000 + "ms");
  }
}
