import java.util.*;
public class FamFix {
  static void t(String n, Runnable r){
    try { r.run(); System.out.println(n+": NO THROW"); }
    catch (Throwable e){ System.out.println(n+": "+e.getClass().getSimpleName()+": "+e.getMessage()); }
  }
  public static void main(String[] a){
    int M = Integer.MAX_VALUE;
    System.out.println("== huge capacity (M=Integer.MAX_VALUE) ==");
    t("HashMap(M)",       () -> { Map<?,?> m=new HashMap<>(M);      if(!m.isEmpty()) throw new IllegalStateException(); });
    t("HashSet(M)",       () -> { Set<?> s=new HashSet<>(M);        if(!s.isEmpty()) throw new IllegalStateException(); });
    t("LinkedHashMap(M)", () -> { Map<?,?> m=new LinkedHashMap<>(M);if(!m.isEmpty()) throw new IllegalStateException(); });
    t("ArrayDeque(M)",    () -> { new ArrayDeque<>(M); });
    t("PriorityQueue(M)", () -> { new PriorityQueue<>(M); });
    t("StringBuilder(M)", () -> { new StringBuilder(M); });

    System.out.println("== normal usage (no regression) ==");
    HashMap<Integer,Integer> hm = new HashMap<>(16);
    for(int i=0;i<100;i++) hm.put(i, i*i);
    System.out.println("HashMap(16)+100puts: size="+hm.size()+" get(50)="+hm.get(50)+" get(99)="+hm.get(99));

    HashMap<Integer,Integer> hmBig = new HashMap<>(M); // capped table, must still work
    for(int i=0;i<1000;i++) hmBig.put(i, -i);
    System.out.println("HashMap(M)+1000puts: size="+hmBig.size()+" get(0)="+hmBig.get(0)+" get(999)="+hmBig.get(999));

    LinkedHashMap<String,Integer> lhm = new LinkedHashMap<>(8);
    lhm.put("c",3); lhm.put("a",1); lhm.put("b",2);
    System.out.println("LinkedHashMap insertion order: "+lhm.keySet());  // expect [c, a, b]

    HashSet<Integer> hs = new HashSet<>(32);
    for(int i=0;i<50;i++){ hs.add(i); hs.add(i); }
    System.out.println("HashSet(32): size="+hs.size()+" contains(25)="+hs.contains(25));

    ArrayDeque<Integer> dq = new ArrayDeque<>(4);
    for(int i=0;i<10;i++) dq.addLast(i);
    System.out.println("ArrayDeque(4)+10: size="+dq.size()+" peekFirst="+dq.peekFirst()+" peekLast="+dq.peekLast());

    PriorityQueue<Integer> pq = new PriorityQueue<>(4);
    pq.add(5); pq.add(1); pq.add(3);
    System.out.println("PriorityQueue(4): poll="+pq.poll()+" poll="+pq.poll());

    StringBuilder sb = new StringBuilder(4);
    for(int i=0;i<20;i++) sb.append(i);
    System.out.println("StringBuilder(4)+20: len="+sb.length()+" str="+sb);
    System.out.println("DONE-FAMFIX");
  }
}
