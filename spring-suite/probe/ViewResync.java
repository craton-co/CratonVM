// Exercise the resync-rebuild + collect_view_snapshot paths on a view backing,
// which the synthetic-layout consumers handle and which my real-layout change
// must NOT break ("both modes should work").
import java.util.*;
import java.util.stream.*;
public class ViewResync {
  static int fails=0;
  static void ck(String n, boolean ok, Object g, Object e){ System.out.println((ok?"OK  ":"FAIL")+" "+n+(ok?"":"  got="+g+" exp="+e)); if(!ok)fails++; }
  public static void main(String[] a){
    HashMap<Integer,Integer> m = new HashMap<>();
    for(int i=0;i<8;i++) m.put(i,i*7);
    Set<Integer> ks = m.keySet();

    // 1) trigger resync via several reads, THEN spliterate
    ks.size(); ks.contains(3); { Iterator<Integer> it=ks.iterator(); while(it.hasNext()) it.next(); }
    int[] c={0}; Spliterator<Integer> s=ks.spliterator(); while(s.tryAdvance((Integer k)->c[0]++)){}
    ck("spliterator AFTER resync reads", c[0]==8, c[0], 8);

    // 2) live-view: mutate SOURCE after obtaining keySet, then read view
    m.put(100, 700);
    ck("live-view size reflects source put", ks.size()==9, ks.size(), 9);
    ck("live-view contains new key", ks.contains(100), ks.contains(100), true);
    int[] c2={0}; Spliterator<Integer> s2=ks.spliterator(); while(s2.tryAdvance((Integer k)->c2[0]++)){}
    ck("spliterator after live mutate", c2[0]==9, c2[0], 9);

    // 3) collect_view_snapshot: new ArrayList<>(keySet) and toArray
    ArrayList<Integer> copy = new ArrayList<>(ks);
    ck("new ArrayList<>(keySet) size", copy.size()==9, copy.size(), 9);
    Object[] arr = ks.toArray();
    ck("keySet.toArray length", arr.length==9, arr.length, 9);
    int sum=0; for(int k: ks) sum+=k;
    ck("keySet sum after mutate", sum==(0+1+2+3+4+5+6+7+100), sum, 28+100);

    // 4) entrySet resync + setValue write-through
    Map.Entry<Integer,Integer> first=null;
    for (Map.Entry<Integer,Integer> e : m.entrySet()) { if (e.getKey()==2) first=e; }
    if (first!=null) { first.setValue(999); ck("entrySet.setValue write-through", m.get(2)==999, m.get(2), 999); }
    else ck("entrySet found key 2", false, null, "entry");

    // 5) repeated keySet spliterator (fresh each time)
    boolean allEq=true; for(int t=0;t<3;t++){ int[] cc={0}; Spliterator<Integer> sp=m.keySet().spliterator(); while(sp.tryAdvance((Integer k)->cc[0]++)){} if(cc[0]!=9) allEq=false; }
    ck("repeated fresh keySet spliterator", allEq, allEq, true);

    System.out.println(fails==0?"ALL PASS":(fails+" FAILURES"));
  }
}
