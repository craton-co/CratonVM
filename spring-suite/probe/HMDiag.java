// Diagnose where HashMap spliterator diverges under cratonvm --nojit.
import java.util.*;
public class HMDiag {
  public static void main(String[] a) throws Exception {
    HashMap<Integer,Integer> m = new HashMap<>();
    for (int i = 0; i < 8; i++) m.put(i, i*7);
    System.out.println("size=" + m.size());
    System.out.println("get(3)=" + m.get(3));

    // 1) plain keySet iterator (Iterator path)
    int c1 = 0; for (Integer k : m.keySet()) c1++;
    System.out.println("keySet iterator count=" + c1);

    // 2) reflective read of the real `table` field
    try {
      java.lang.reflect.Field f = HashMap.class.getDeclaredField("table");
      f.setAccessible(true);
      Object tab = f.get(m);
      System.out.println("reflect table class=" + (tab==null?"null":tab.getClass().getName())
        + " len=" + (tab==null?-1:java.lang.reflect.Array.getLength(tab)));
    } catch (Throwable t) { System.out.println("reflect table FAILED: " + t); }

    // 3) the failing path: spliterator().tryAdvance
    try {
      Spliterator<Integer> s = m.keySet().spliterator();
      final int[] c = {0};
      while (s.tryAdvance((Integer k) -> c[0]++)) {}
      System.out.println("spliterator tryAdvance count=" + c[0]);
    } catch (Throwable t) { System.out.println("spliterator FAILED: " + t); }
  }
}
