// Scope check: do other map types' keySet/entrySet spliterators work?
import java.util.*;
import java.util.concurrent.*;
public class OtherMaps {
  static int n(Map<Integer,Integer> m){ int[] c={0}; try{ Spliterator<Integer> s=m.keySet().spliterator(); while(s.tryAdvance((Integer k)->c[0]++)){} return c[0]; }catch(Throwable t){ return -1; } }
  static int ne(Map<Integer,Integer> m){ int[] c={0}; try{ Spliterator<Map.Entry<Integer,Integer>> s=m.entrySet().spliterator(); while(s.tryAdvance(x->c[0]++)){} return c[0]; }catch(Throwable t){ return -1; } }
  public static void main(String[] a){
    Map<Integer,Integer> lhm=new LinkedHashMap<>(); for(int i=0;i<8;i++) lhm.put(i,i);
    Map<Integer,Integer> tm=new TreeMap<>();       for(int i=0;i<8;i++) tm.put(i,i);
    Map<Integer,Integer> chm=new ConcurrentHashMap<>(); for(int i=0;i<8;i++) chm.put(i,i);
    System.out.println("LinkedHashMap keySet="+n(lhm)+" entrySet="+ne(lhm)+" (exp 8/8)");
    System.out.println("TreeMap        keySet="+n(tm)+" entrySet="+ne(tm)+" (exp 8/8)");
    System.out.println("ConcurrentHM   keySet="+n(chm)+" entrySet="+ne(chm)+" (exp 8/8)");
  }
}
