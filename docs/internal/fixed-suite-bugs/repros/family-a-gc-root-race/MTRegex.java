// spring-bug-10 heavy-load repro: hammer java.util.regex.Matcher (which JITs
// Matcher.reset, the crashing method) on many threads while allocating to drive
// concurrent young GC under CRATONVM_SHADOW_STACK. Goal: reproduce the
// rax=0x3D-in-Matcher.reset SIGSEGV far faster than the 14-class aspectj batch.
import java.util.regex.*;
public class MTRegex {
  static volatile boolean stop = false;
  public static void main(String[] a) throws Exception {
    int nthreads = a.length > 0 ? Integer.parseInt(a[0]) : 8;
    int seconds  = a.length > 1 ? Integer.parseInt(a[1]) : 60;
    final Pattern p = Pattern.compile("([a-z]+)(\\d+)-([A-Z]+)?");
    final String[] inputs = {
      "abc123-XYZ", "hello42", "foo7-BAR", "x9-Q", "longword12345-ABCDEF",
      "no match here", "a1-B", "zzz999-ZZZ", "q0-A", "mix3d-Up"
    };
    Thread[] ts = new Thread[nthreads];
    final long[] counts = new long[nthreads];
    for (int t = 0; t < nthreads; t++) {
      final int id = t;
      ts[t] = new Thread(() -> {
        long c = 0;
        java.util.ArrayList<int[]> garbage = new java.util.ArrayList<>();
        while (!stop) {
          for (String s : inputs) {
            Matcher m = p.matcher(s + c);
            if (m.find() && m.group(0) != null) c++;
            m.reset();                   // bare reset() — the crashing method
            m.reset(s);                  // reset(CharSequence) -> reset()
            // allocate to push young GC
            garbage.add(new int[16]);
            if (garbage.size() > 4096) garbage.clear();
          }
        }
        counts[id] = c;
      });
      ts[t].start();
    }
    Thread.sleep(seconds * 1000L);
    stop = true;
    long total = 0;
    for (int t = 0; t < nthreads; t++) { ts[t].join(); total += counts[t]; }
    System.out.println("DONE threads=" + nthreads + " total=" + total);
  }
}
