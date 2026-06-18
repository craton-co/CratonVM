// Stress: hold Class refs across heavy GC, then use isInstance/cast repeatedly.
// Reproduces "stale all-zero header" if Class mirrors aren't kept live/remapped.
import java.util.*;
public class ClassGc {
  static Class<?>[] held;
  public static void main(String[] a){
    Class<?>[] cs = { String.class, Integer.class, ArrayList.class, HashMap.class,
                      Object.class, Long.class, Double.class, int[].class,
                      Iterator.class, Map.Entry.class };
    held = cs;
    Object[] probes = { "x", 1, new ArrayList<>(), new HashMap<>(), new Object(),
                        2L, 3.0, new int[1], new ArrayList<>().iterator() };
    long ok=0;
    for(int round=0; round<2000; round++){
      // churn garbage to force GC cycles
      for(int i=0;i<5000;i++){ Object junk = new byte[256]; if(junk.hashCode()==42) held=cs; }
      // use the held Class mirrors after GC
      for(Class<?> c: cs){
        for(Object p: probes){
          if(c.isInstance(p)){ Object z=c.cast(p); ok += z==null?0:1; }
        }
        String n = c.getName(); if(n.isEmpty()) throw new RuntimeException("empty");
      }
    }
    System.out.println("ClassGc DONE ok="+ok);
  }
}
