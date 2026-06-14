import java.util.regex.*;
public class RegexBench3 {
  public static void main(String[] a) {
    String cls = "org.junit.jupiter.engine.descriptor.ClassBasedTestDescriptor";
    Pattern p = Pattern.compile("[.]");
    Matcher m = p.matcher(cls);
    for (int i=0;i<5000;i++){ m.reset(cls); m.replaceAll("/"); }   // warmup, reused matcher
    long t0=System.nanoTime(); String s=null;
    for (int i=0;i<5000;i++){ m.reset(cls); s=m.replaceAll("/"); }
    long t1=System.nanoTime();
    System.out.println("reused-matcher x5000 = "+(t1-t0)/1_000_000+"ms");
    // also: plain indexOf/replace (no regex) baseline
    long t2=System.nanoTime();
    for (int i=0;i<5000;i++){ s=cls.replace('.','/'); }
    long t3=System.nanoTime();
    System.out.println("String.replace(char) x5000 = "+(t3-t2)/1_000_000+"ms");
  }
}
