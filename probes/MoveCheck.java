import java.util.*;

/** Exercises the things a MOVING collector must keep coherent:
 *  statics, monitors on long-lived objects, reference arrays, and identity. */
public class MoveCheck {
  static Object[] STATIC_ARRAY = new Object[64];
  static String STATIC_STRING;
  static Map<String,Object> STATIC_MAP = new HashMap<>();
  static final Object LOCK_A = new Object();
  static Object lockB;

  static int churn(int n) {
    int h = 0;
    for (int i = 0; i < n; i++) { byte[] b = new byte[256]; h += b.length; }
    return h;
  }

  public static void main(String[] a) throws Exception {
    for (int i = 0; i < 64; i++) STATIC_ARRAY[i] = new int[]{i, i*2, i*3};
    STATIC_STRING = "sentinel-" + 12345;
    for (int i = 0; i < 200; i++) STATIC_MAP.put("k"+i, new int[]{i});
    lockB = new Object();

    // Hold monitors on objects that will survive collections.
    synchronized (LOCK_A) { synchronized (lockB) { churn(1000); } }

    for (int g = 0; g < 12; g++) {
      churn(20000);
      System.gc();
      // Re-enter monitors AFTER a possible slide: if a live object's monitor
      // was freed by the prune, this is where it bites.
      synchronized (LOCK_A) { churn(50); }
      synchronized (lockB) { churn(50); }
      synchronized (STATIC_MAP) { churn(50); }
    }

    int bad = 0;
    for (int i = 0; i < 64; i++) {
      int[] v = (int[]) STATIC_ARRAY[i];
      if (v == null || v.length != 3 || v[0] != i || v[1] != i*2 || v[2] != i*3) bad++;
    }
    if (!"sentinel-12345".equals(STATIC_STRING)) bad += 1000;
    for (int i = 0; i < 200; i++) {
      int[] v = (int[]) STATIC_MAP.get("k"+i);
      if (v == null || v[0] != i) bad++;
    }
    System.out.println("MOVECHECK bad=" + bad + " mapSize=" + STATIC_MAP.size()
        + " str=" + STATIC_STRING + " hashA=" + (System.identityHashCode(LOCK_A) != 0));
  }
}
