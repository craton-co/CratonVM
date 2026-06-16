// spring-bug-11 isolation: HashMap$KeySpliterator.tryAdvance JIT miscompile.
// The Groovy SIGSEGV's faulting frame is this JDK method (dup_x1 at pc 71 in the
// `current = tab[index++]` idiom). Drive it hot to force JIT, then tryAdvance the
// whole map; compare the consumed-key count to HotSpot.
import java.util.*;
import java.util.concurrent.atomic.AtomicLong;
public class KSplProbe {
  public static void main(String[] a) {
    long sum = 0;
    for (int iter = 0; iter < 200_000; iter++) {
      HashMap<Integer,Integer> m = new HashMap<>();
      for (int i = 0; i < 8; i++) m.put(i, i * 7);
      Spliterator<Integer> s = m.keySet().spliterator();
      final long[] acc = {0};
      // drive tryAdvance one element at a time over the whole map
      while (s.tryAdvance((Integer k) -> acc[0] += k)) { }
      sum += acc[0];
      if ((iter & 0xFFFF) == 0) System.out.println("iter=" + iter + " sum=" + sum);
    }
    System.out.println("FINAL sum=" + sum + " (expect " + (200_000L * 28) + ")");
  }
}
