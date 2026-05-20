import java.util.*; import java.util.stream.*;
public class SpliterProbe {
  public static void main(String[] a) {
    Stream<Integer> s = Stream.of(1,2,3,4);
    Spliterator<Integer> sp = s.spliterator();
    long n = sp.estimateSize();
    int[] cnt = {0};
    sp.forEachRemaining(x -> cnt[0]++);
    System.out.println("est=" + n + " count=" + cnt[0]);
    // also via collection:
    List<Integer> l = List.of(5,6,7);
    Spliterator<Integer> sp2 = l.stream().spliterator();
    int[] c2 = {0}; sp2.forEachRemaining(x -> c2[0]++);
    System.out.println("count2=" + c2[0]);
  }
}
