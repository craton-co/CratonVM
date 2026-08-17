import java.util.*;

/** A LARGE LIVE SET, which is the condition Phase 3's exit names.
 *  Retains a deep/wide object graph so the mark phase dominates the pause,
 *  then forces collections over it. */
public class BigLive {
  static Object[] retained;
  public static void main(String[] a) {
    int width = Integer.parseInt(a[0]);   // top-level slots
    int depth = Integer.parseInt(a[1]);   // chain length per slot
    retained = new Object[width];
    for (int i = 0; i < width; i++) {
      Object[] head = new Object[4];
      Object[] cur = head;
      for (int d = 0; d < depth; d++) {
        Object[] next = new Object[4];
        next[0] = new byte[64];
        cur[1] = next;
        cur = next;
      }
      retained[i] = head;
    }
    long live = (long) width * depth;
    System.out.println("live nodes ~" + live);
    for (int g = 0; g < 6; g++) {
      // a little garbage so the cycle has something to reclaim
      for (int k = 0; k < 20000; k++) { Object o = new byte[128]; if (o == null) return; }
      System.gc();
    }
    System.out.println("done, retained[0]!=null=" + (retained[0] != null));
  }
}
