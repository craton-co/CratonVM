import java.util.*;
import java.util.stream.*;
public class IsoToMap {
  public static void main(String[] x){
    Map<Integer,Integer> tm = IntStream.range(0,10).boxed().collect(Collectors.toMap(i->i, i->i*i));
    System.out.println("tm class=" + tm.getClass().getName() + " size=" + tm.size() + " get(5)=" + tm.get(5));
    Set<Integer> ks = tm.keySet();
    System.out.println("keySet class=" + ks.getClass().getName() + " size=" + ks.size());
    System.out.print("for-each keys: ");
    int n=0; for (Integer k : ks) { System.out.print(k+" "); n++; } System.out.println(" (n="+n+")");
    System.out.print("iterator keys: ");
    Iterator<Integer> it=ks.iterator(); int m=0; while(it.hasNext()){ Integer k=it.next(); System.out.print(k+(k==null?"(NULL)":"")+" "); m++; } System.out.println(" (m="+m+")");
    int[] c={0}; boolean[] hadNull={false};
    Spliterator<Integer> sp=ks.spliterator(); while(sp.tryAdvance((Integer k)->{ if(k==null)hadNull[0]=true; c[0]++; })){}
    System.out.println("spliterator count="+c[0]+" hadNull="+hadNull[0]);
    // also a plain HashMap built same way
    HashMap<Integer,Integer> hm = new HashMap<>(); for(int i=0;i<10;i++) hm.put(i,i*i);
    System.out.println("plain HashMap keySet sum=" + hm.keySet().stream().mapToInt(i->i).sum());
  }
}
