// spring-bug-11: drive a REAL java.util.HashMap KeySpliterator.tryAdvance HOT so
// its internal bucket-skip loop (which contains the `tab[i++]` dup_x1 idiom) OSR-
// compiles. A sparse table (few entries, many empty buckets) maximizes the inner
// loop trip count. Compare the summed count to HotSpot; a JIT dup_x1/OSR miscompile
// shows as a wrong count or a SIGSEGV.
import java.util.*;
public class HMSplHot {
  public static void main(String[] a) {
    HashMap<Integer,Integer> m = new HashMap<>();
    // Sparse: 24 entries spread so the spliterator skips many empty buckets.
    for (int i = 0; i < 24; i++) m.put(i * 37, i);
    long sum = 0;
    long perPass = -1;
    for (int it = 0; it < 8_000_000; it++) {
      long c = 0;
      Spliterator<Integer> s = m.keySet().spliterator();
      // tight tryAdvance loop — the spliterator's internal bucket walk OSR-compiles.
      final long[] box = {0};
      while (s.tryAdvance((Integer k) -> box[0]++)) {}
      c = box[0];
      if (perPass < 0) perPass = c;
      sum += c;
      if ((it & 0xFFFFF) == 0) System.out.println("it=" + it + " count=" + c);
    }
    System.out.println("FINAL sum=" + sum + " perPass=" + perPass);
  }
}
