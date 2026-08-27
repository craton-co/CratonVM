import java.lang.reflect.Field;
import java.util.HashMap;

/**
 * Is `modCount` really frozen, or is reflection reading the wrong slot?
 * `size` and `threshold` are the controls: both move on a real HashMap, so a
 * run where they track and `modCount` does not has a frozen counter, not a
 * broken reader.
 */
public class MapFieldProbe {
    public static void main(String[] args) throws Exception {
        Field size = HashMap.class.getDeclaredField("size");
        Field modCount = HashMap.class.getDeclaredField("modCount");
        Field threshold = HashMap.class.getDeclaredField("threshold");
        size.setAccessible(true);
        modCount.setAccessible(true);
        threshold.setAccessible(true);

        HashMap<Integer, Object> m = new HashMap<>();
        for (int i = 0; i <= 6; i++) {
            if (i > 0) {
                m.put(1000 + i * 7, new Object());
            }
            System.out.println("puts=" + i
                    + " api.size=" + m.size()
                    + " field.size=" + size.get(m)
                    + " field.modCount=" + modCount.get(m)
                    + " field.threshold=" + threshold.get(m));
        }
        m.remove(1007);
        System.out.println("after remove: api.size=" + m.size()
                + " field.size=" + size.get(m)
                + " field.modCount=" + modCount.get(m));
        m.clear();
        System.out.println("after clear:  api.size=" + m.size()
                + " field.size=" + size.get(m)
                + " field.modCount=" + modCount.get(m));
    }
}
