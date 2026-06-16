// Direct map-view operations my change touches (no Stream/Collectors).
import java.util.*;
public class FocusedReg {
  static int fails=0;
  static void ck(String n, boolean ok, Object g){ System.out.println((ok?"OK  ":"FAIL")+" "+n+(ok?"":"  got="+g)); if(!ok)fails++; }
  public static void main(String[] x){
    // putAll between HashMaps + entrySet iteration
    HashMap<String,Integer> h = new HashMap<>(); h.put("x",1); h.put("y",2);
    HashMap<String,Integer> h2 = new HashMap<>(); h2.putAll(h); h2.put("z",3);
    ck("putAll+put size", h2.size()==3, h2.size());
    int esum=0; for (Map.Entry<String,Integer> e : h2.entrySet()) esum += e.getValue();
    ck("entrySet value sum", esum==6, esum);

    // computeIfAbsent / merge (direct map ops)
    HashMap<String,List<Integer>> mm = new HashMap<>();
    for (int i=0;i<6;i++) mm.computeIfAbsent(i%2==0?"e":"o", k->new ArrayList<>()).add(i);
    ck("computeIfAbsent groups", mm.get("e").size()==3 && mm.get("o").size()==3, mm);
    HashMap<String,Integer> cnt = new HashMap<>();
    for (String w : "a b a c b a".split(" ")) cnt.merge(w,1,Integer::sum);
    ck("merge counts", cnt.get("a")==3 && cnt.get("b")==2 && cnt.get("c")==1, cnt);

    // keySet().removeIf write-through (uses the view + remove)
    HashMap<Integer,Integer> r = new HashMap<>(); for(int i=0;i<10;i++) r.put(i,i);
    r.keySet().removeIf(k->k%2==0);
    ck("keySet.removeIf write-through", r.size()==5 && !r.containsKey(4) && r.containsKey(5), r.size());

    // entrySet().removeIf + values().remove write-through
    HashMap<Integer,Integer> r2 = new HashMap<>(); for(int i=0;i<10;i++) r2.put(i,i*10);
    r2.entrySet().removeIf(e->e.getValue()>=50);
    ck("entrySet.removeIf write-through", r2.size()==5, r2.size());
    r2.values().remove(20);
    ck("values.remove write-through", !r2.containsKey(2), r2.containsKey(2));

    // new HashMap<>(otherMap) copy constructor (iterates entrySet)
    HashMap<Integer,Integer> src = new HashMap<>(); for(int i=0;i<5;i++) src.put(i,i+100);
    HashMap<Integer,Integer> cp = new HashMap<>(src);
    ck("copy ctor size", cp.size()==5 && cp.get(3)==103, cp.size());

    // keySet/values/entrySet toArray + contains
    HashMap<Integer,Integer> q = new HashMap<>(); for(int i=0;i<7;i++) q.put(i,i);
    ck("keySet contains", q.keySet().contains(5) && !q.keySet().contains(99), null);
    ck("keySet toArray len", q.keySet().toArray().length==7, q.keySet().toArray().length);
    ck("values toArray len", q.values().toArray().length==7, q.values().toArray().length);
    ck("entrySet toArray len", q.entrySet().toArray().length==7, q.entrySet().toArray().length);

    System.out.println(fails==0?"ALL PASS":(fails+" FAILURES"));
  }
}
