// Validate the keySet/entrySet/values view fix + write-through semantics.
import java.util.*;
import java.util.stream.*;
public class CollViewTest {
  static int fails = 0;
  static void check(String name, boolean ok, Object got, Object exp) {
    System.out.println((ok?"OK  ":"FAIL")+" "+name+(ok?"":"  got="+got+" exp="+exp));
    if (!ok) fails++;
  }
  public static void main(String[] a) {
    HashMap<Integer,Integer> m = new HashMap<>();
    for (int i=0;i<8;i++) m.put(i, i*7);

    // 1) keySet spliterator (the reported bug)
    long c1=0; for (Spliterator<Integer> s=m.keySet().spliterator(); s.tryAdvance((Integer k)->{}); ) {}
    int[] kc={0}; Spliterator<Integer> ks=m.keySet().spliterator(); while(ks.tryAdvance((Integer k)->kc[0]++)){}
    check("keySet.spliterator count", kc[0]==8, kc[0], 8);

    // 2) keySet stream sum
    int ksum = m.keySet().stream().mapToInt(Integer::intValue).sum();
    check("keySet.stream sum", ksum==(0+1+2+3+4+5+6+7), ksum, 28);

    // 3) entrySet spliterator
    int[] ec={0}; Spliterator<Map.Entry<Integer,Integer>> es=m.entrySet().spliterator();
    while(es.tryAdvance(x->ec[0]++)){}
    check("entrySet.spliterator count", ec[0]==8, ec[0], 8);

    // 4) values spliterator
    int[] vc={0}; Spliterator<Integer> vs=m.values().spliterator(); while(vs.tryAdvance(x->vc[0]++)){}
    check("values.spliterator count", vc[0]==8, vc[0], 8);

    // 5) entrySet stream: sum of values
    int vsum = m.entrySet().stream().mapToInt(Map.Entry::getValue).sum();
    check("entrySet value sum", vsum==(7*(0+1+2+3+4+5+6+7)), vsum, 7*28);

    // 6) WRITE-THROUGH: keySet().remove(k) must remove from the map
    HashMap<Integer,Integer> m2 = new HashMap<>();
    for (int i=0;i<5;i++) m2.put(i, i);
    boolean removed = m2.keySet().remove(2);
    check("keySet.remove returns true", removed, removed, true);
    check("keySet.remove write-through (map size)", m2.size()==4, m2.size(), 4);
    check("keySet.remove write-through (containsKey)", !m2.containsKey(2), m2.containsKey(2), false);

    // 7) WRITE-THROUGH: keySet().iterator().remove()
    HashMap<Integer,Integer> m3 = new HashMap<>();
    for (int i=0;i<5;i++) m3.put(i, i);
    Iterator<Integer> it = m3.keySet().iterator();
    while (it.hasNext()) { int k=it.next(); if (k==3) it.remove(); }
    check("keySet iterator.remove write-through", m3.size()==4 && !m3.containsKey(3), m3.size(), 4);

    // 8) plain HashSet still works (no regression)
    HashSet<Integer> hs = new HashSet<>(Arrays.asList(10,20,30));
    int[] hc={0}; Spliterator<Integer> hss=hs.spliterator(); while(hss.tryAdvance(x->hc[0]++)){}
    check("plain HashSet.spliterator count", hc[0]==3, hc[0], 3);

    // 9) keySet iteration still correct values
    int ksum2 = 0; for (int k : m.keySet()) ksum2 += k;
    check("keySet for-each sum", ksum2==28, ksum2, 28);

    System.out.println(fails==0 ? "ALL PASS" : (fails+" FAILURES"));
  }
}
