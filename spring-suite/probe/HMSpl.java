// Inspect the KeySpliterator's `map` field directly (no rebuild).
import java.util.*;
import java.lang.reflect.*;
public class HMSpl {
  static void dump(String tag, Object o) {
    System.out.println(tag + ": class=" + (o==null?"null":o.getClass().getName())
      + " identity=" + System.identityHashCode(o));
  }
  public static void main(String[] a) throws Exception {
    HashMap<Integer,Integer> m = new HashMap<>();
    for (int i = 0; i < 8; i++) m.put(i, i*7);
    dump("m", m);

    Spliterator<Integer> s = m.keySet().spliterator();
    dump("spliterator", s);

    System.out.println("spliterator fields (incl. superclasses):");
    java.util.List<Field> allf = new ArrayList<>();
    for (Class<?> sc = s.getClass(); sc != null && sc != Object.class; sc = sc.getSuperclass()) {
      System.out.println("  [class " + sc.getName() + "]");
      for (Field f : sc.getDeclaredFields()) allf.add(f);
    }
    for (Field f : allf) {
      if (Modifier.isStatic(f.getModifiers())) continue;
      f.setAccessible(true);
      Object v;
      try { v = f.get(s); } catch (Throwable t) { v = "<err:"+t+">"; }
      String d = v==null?"null":(v.getClass().isArray()? v.getClass().getName()+"[len="+Array.getLength(v)+"]"
          : v.getClass().getName()+"@"+System.identityHashCode(v));
      System.out.println("  " + f.getType().getSimpleName() + " " + f.getName() + " = " + d);
      if (f.getName().equals("map")) {
        System.out.println("    -> map IS m? " + (v == m));
        if (v != null) {
          try {
            Field ft = v.getClass().getDeclaredField("table");
            ft.setAccessible(true);
            Object tab = ft.get(v);
            System.out.println("    -> map.table = " + (tab==null?"null":tab.getClass().getName()
              + "[len=" + Array.getLength(tab) + "]"));
          } catch (Throwable t) { System.out.println("    -> map.table read FAILED: " + t); }
        }
      }
    }
  }
}
