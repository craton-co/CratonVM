import java.util.*;
public class MapProbe {
  public static void main(String[] a){
    Map<String,Integer> m = new HashMap<>(); m.put("x",1); m.put("y",2);
    System.out.println("hashmap size="+m.size()+" get="+m.get("x")+" cap-ok");
    Map<String,Integer> e = Collections.emptyMap();
    System.out.println("emptymap size="+e.size()+" get="+e.get("z"));
    Map<String,Integer> s = Collections.singletonMap("k",9);
    System.out.println("singleton size="+s.size()+" get="+s.get("k"));
    LinkedHashMap<String,Integer> lh = new LinkedHashMap<>(); lh.put("a",1);
    System.out.println("linkedhashmap size="+lh.size());
    System.out.println("MAPPROBE_OK");
  }
}
