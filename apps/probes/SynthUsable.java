import java.util.*;
import java.util.concurrent.*;
public class SynthUsable {
  public static void main(String[] a) {
    row("CopyOnWriteArrayList", () -> { List<String> l = new CopyOnWriteArrayList<>(); l.add("x"); return l.size()+"/"+l.get(0)+"/"+it(l); });
    row("ConcurrentSkipListSet", () -> { Set<String> s = new ConcurrentSkipListSet<>(); s.add("x"); return s.size()+"/"+s.contains("x")+"/"+it(s); });
    row("ConcurrentSkipListMap", () -> { Map<String,String> m = new ConcurrentSkipListMap<>(); m.put("k","v"); return m.size()+"/"+m.get("k"); });
    row("Vector", () -> { List<String> l = new Vector<>(); l.add("x"); return l.size()+"/"+l.get(0)+"/"+it(l); });
    row("ArrayDeque", () -> { Deque<String> d = new ArrayDeque<>(); d.add("x"); return d.size()+"/"+d.peekFirst()+"/"+it(d); });
    row("PriorityQueue", () -> { Queue<String> q = new PriorityQueue<>(); q.add("b"); q.add("a"); return q.size()+"/"+q.poll()+"/"+q.poll(); });
    row("Collections.unmodifiableSortedMap", () -> Collections.unmodifiableSortedMap(new TreeMap<>(Map.of("a","b"))).toString());
  }
  static String it(Collection<?> c) { int n=0; for (Object o : c) n++; return "iter="+n; }
  interface B { String run() throws Exception; }
  static void row(String l, B b) {
    String v; try { v = b.run(); } catch (Throwable t) { v = "ERROR " + t.getClass().getSimpleName() + ": " + t.getMessage(); }
    System.out.println(l + " | " + v);
  }
}
