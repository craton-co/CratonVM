import java.util.*;
import java.util.stream.*;
public class MapsRegression {
  static int fails=0;
  static void ck(String n, boolean ok, Object g){ System.out.println((ok?"OK  ":"FAIL")+" "+n+(ok?"":"  got="+g)); if(!ok)fails++; }
  public static void main(String[] x){
    // immutable Map.of / Set.of (real-layout HashSet path)
    Map<String,Integer> mof = Map.of("a",1,"b",2,"c",3);
    ck("Map.of keySet size", mof.keySet().size()==3, mof.keySet().size());
    ck("Map.of keySet spliterator", mof.keySet().stream().count()==3, mof.keySet().stream().count());
    Set<Integer> sof = Set.of(1,2,3,4);
    ck("Set.of stream sum", sof.stream().mapToInt(i->i).sum()==10, sof.stream().mapToInt(i->i).sum());

    // Collectors.toMap then iterate
    Map<Integer,Integer> tm = IntStream.range(0,10).boxed().collect(Collectors.toMap(i->i, i->i*i));
    ck("toMap size", tm.size()==10, tm.size());
    int kssum = tm.keySet().stream().mapToInt(i->i).sum();
    ck("toMap keySet sum", kssum==45, kssum);
    int vssum = tm.values().stream().mapToInt(i->i).sum();
    ck("toMap values sum", vssum==285, vssum);

    // groupingBy
    Map<Boolean,List<Integer>> g = IntStream.range(0,10).boxed().collect(Collectors.groupingBy(i->i%2==0));
    ck("groupingBy even count", g.get(true).size()==5, g.get(true).size());

    // putAll / entrySet mutation
    HashMap<String,Integer> h = new HashMap<>(); h.put("x",1); h.put("y",2);
    HashMap<String,Integer> h2 = new HashMap<>(); h2.putAll(h); h2.put("z",3);
    ck("putAll+put size", h2.size()==3, h2.size());
    int esum=0; for (Map.Entry<String,Integer> e : h2.entrySet()) esum += e.getValue();
    ck("entrySet value sum", esum==6, esum);

    // computeIfAbsent / merge
    HashMap<String,List<Integer>> mm = new HashMap<>();
    for (int i=0;i<6;i++) mm.computeIfAbsent(i%2==0?"e":"o", k->new ArrayList<>()).add(i);
    ck("computeIfAbsent groups", mm.get("e").size()==3 && mm.get("o").size()==3, mm);
    HashMap<String,Integer> cnt = new HashMap<>();
    for (String w : "a b a c b a".split(" ")) cnt.merge(w,1,Integer::sum);
    ck("merge counts", cnt.get("a")==3 && cnt.get("b")==2 && cnt.get("c")==1, cnt);

    // keySet().removeIf write-through
    HashMap<Integer,Integer> r = new HashMap<>(); for(int i=0;i<10;i++) r.put(i,i);
    r.keySet().removeIf(k->k%2==0);
    ck("keySet.removeIf write-through", r.size()==5 && !r.containsKey(4) && r.containsKey(5), r.size());

    // nested map + toString sanity
    Map<String,Map<String,Integer>> nest = new HashMap<>();
    nest.put("a", Map.of("x",1)); nest.put("b", Map.of("y",2));
    int ns=0; for (Map<String,Integer> inner : nest.values()) ns += inner.values().stream().mapToInt(i->i).sum();
    ck("nested values sum", ns==3, ns);

    System.out.println(fails==0?"ALL PASS":(fails+" FAILURES"));
  }
}
