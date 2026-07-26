import org.mockito.Mockito;

/** Measure the per-call cost of a REAL StringBuilder.length() before vs after
 *  Mockito.mock(StringBuilder.class) has woven MockMethodAdvice into
 *  AbstractStringBuilder.length(). javac's tokenizer calls length() per token,
 *  so a large multiplier here is what turns Spring's AOT TestCompiler step into
 *  an apparent hang. */
public class SbCostProbe {
  static long timeLength(StringBuilder sb, int iters) {
    long t0 = System.nanoTime();
    long acc = 0;
    for (int i = 0; i < iters; i++) acc += sb.length();
    long dt = System.nanoTime() - t0;
    if (acc == -1) System.out.println("never");
    return dt;
  }

  public static void main(String[] a) {
    int iters = Integer.getInteger("iters", 2_000_000);
    StringBuilder sb = new StringBuilder("hello world");

    timeLength(sb, iters / 10);              // warm
    long before = timeLength(sb, iters);
    System.out.println("BEFORE  " + iters + " length() calls: " + (before / 1_000_000) + " ms"
        + "  (" + (before / iters) + " ns/call)  len=" + sb.length());

    StringBuilder mock = Mockito.mock(StringBuilder.class);
    System.out.println("mocked=" + (mock != null));

    StringBuilder sb2 = new StringBuilder("hello world");
    timeLength(sb2, iters / 10);             // warm the woven path
    long after = timeLength(sb2, iters);
    System.out.println("AFTER   " + iters + " length() calls: " + (after / 1_000_000) + " ms"
        + "  (" + (after / iters) + " ns/call)  len=" + sb2.length());

    System.out.println("MULTIPLIER " + (before == 0 ? -1 : (after / Math.max(1, before))) + "x");
  }
}
