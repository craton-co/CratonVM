import java.util.*;
import java.util.concurrent.*;

/** What `ConcurrentHashMap.elements()` actually yields, element by element.
 *  `ChmShadowSweep` reports only "NEVER TERMINATED"; this says WHAT it kept
 *  handing back, which is the difference between "the cursor never advances"
 *  and "the cursor walks a chain that loops". */
public class ChmElemDbg {
    public static void main(String[] args) {
        ConcurrentHashMap<String, Integer> m = new ConcurrentHashMap<>();
        m.put("a", 1); m.put("b", 2); m.put("c", 3);

        Enumeration<Integer> e = m.elements();
        System.out.println("elements class    |" + e.getClass().getName() + "|");
        for (int i = 0; i < 8; i++) {
            boolean h;
            try { h = e.hasMoreElements(); }
            catch (Throwable x) { System.out.println("h" + i + " THREW " + x); break; }
            System.out.println("hasMoreElements " + i + " |" + h + "|");
            if (!h) break;
            try { System.out.println("nextElement " + i + "     |" + e.nextElement() + "|"); }
            catch (Throwable x) { System.out.println("n" + i + " THREW " + x); break; }
        }

        Enumeration<String> k = m.keys();
        System.out.println("keys class        |" + k.getClass().getName() + "|");
        int kn = 0;
        while (k.hasMoreElements() && kn < 8) { k.nextElement(); kn++; }
        System.out.println("keys drained      |" + kn + "|");

        Iterator<Integer> vi = m.values().iterator();
        System.out.println("values itr class  |" + vi.getClass().getName() + "|");
        int vn = 0;
        while (vi.hasNext() && vn < 8) { vi.next(); vn++; }
        System.out.println("values drained    |" + vn + "|");
        System.out.println("DONE ChmElemDbg");
    }
}
