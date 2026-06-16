// Dump CratonVM's actual java.util.HashMap declared-field layout + values.
import java.util.*;
import java.lang.reflect.*;
public class HMFields {
  public static void main(String[] a) throws Exception {
    HashMap<Integer,Integer> m = new HashMap<>();
    for (int i = 0; i < 8; i++) m.put(i, i*7);
    Class<?> c = HashMap.class;
    System.out.println("HashMap superclass=" + c.getSuperclass());
    Field[] fs = c.getDeclaredFields();
    System.out.println("declared field count=" + fs.length);
    for (int i = 0; i < fs.length; i++) {
      Field f = fs[i];
      if (Modifier.isStatic(f.getModifiers())) { System.out.println("  ["+i+"] static "+f.getName()); continue; }
      f.setAccessible(true);
      Object v;
      try { v = f.get(m); } catch (Throwable t) { v = "<err:"+t+">"; }
      String desc = v==null?"null":(v.getClass().isArray()? v.getClass().getName()+"[len="+Array.getLength(v)+"]" : v.toString());
      System.out.println("  ["+i+"] "+f.getType().getSimpleName()+" "+f.getName()+" = "+desc);
    }
    // also superclass (AbstractMap) instance fields
    Class<?> sc = c.getSuperclass();
    if (sc != null) {
      System.out.println("superclass "+sc.getName()+" fields:");
      for (Field f : sc.getDeclaredFields()) {
        if (Modifier.isStatic(f.getModifiers())) continue;
        System.out.println("  "+f.getType().getSimpleName()+" "+f.getName());
      }
    }
  }
}
