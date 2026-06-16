import java.util.*;
public class HMSpl2 {
  public static void main(String[] a) {
    // 1) plain HashSet.spliterator() — does the native fire?
    HashSet<Integer> hs = new HashSet<>();
    for (int i=0;i<8;i++) hs.add(i);
    Spliterator<Integer> s1 = hs.spliterator();
    System.out.println("HashSet.spliterator class=" + s1.getClass().getName());
    int[] c1={0}; try { while(s1.tryAdvance((Integer k)->c1[0]++)){} System.out.println("  HashSet count="+c1[0]); }
      catch (Throwable t){ System.out.println("  HashSet spliterator FAILED: "+t); }

    // 2) HashMap.keySet() return class + its spliterator class
    HashMap<Integer,Integer> m = new HashMap<>();
    for (int i=0;i<8;i++) m.put(i, i*7);
    Set<Integer> ks = m.keySet();
    System.out.println("keySet class=" + ks.getClass().getName());
    Spliterator<Integer> s2 = ks.spliterator();
    System.out.println("keySet.spliterator class=" + s2.getClass().getName());
    int[] c2={0}; try { while(s2.tryAdvance((Integer k)->c2[0]++)){} System.out.println("  keySet count="+c2[0]); }
      catch (Throwable t){ System.out.println("  keySet spliterator FAILED: "+t); }

    // 3) entrySet + values spliterator
    try { Spliterator<?> s3=m.entrySet().spliterator(); int[] c3={0}; while(s3.tryAdvance(x->c3[0]++)){} System.out.println("entrySet count="+c3[0]); }
      catch (Throwable t){ System.out.println("entrySet FAILED: "+t); }
    try { Spliterator<?> s4=m.values().spliterator(); int[] c4={0}; while(s4.tryAdvance(x->c4[0]++)){} System.out.println("values count="+c4[0]); }
      catch (Throwable t){ System.out.println("values FAILED: "+t); }
  }
}
