// spring-bug-11: faithful mirror of HashMap$KeySpliterator.tryAdvance to reproduce
// the OSR dup_x1 miscompile in a class I control. Mirrors: map.table indirection,
// getFence-like call, the `current = tab[index++]` dup_x1 idiom, a Consumer.accept
// call, the modCount/CME check, and the same local count.
import java.util.function.IntConsumer;
public class OsrDupX2 {
  static final class Node { Node next; int key; Node(int k){key=k;} }
  static final class Map { Node[] table; int modCount; int size; }

  Map map;
  Node current;
  int index;
  int fence;
  int expectedModCount;

  int getFence() {            // mirrors HashMapSpliterator.getFence
    int hi = fence;
    if (hi < 0) { fence = hi = (map == null) ? 0 : map.table.length; }
    return hi;
  }

  boolean tryAdvance(IntConsumer action) {
    if (action == null) throw new NullPointerException();
    Node[] tab = map.table;                 // local 2
    if (tab != null && tab.length >= getFence() && index >= 0) {
      while (current != null || index < fence) {
        if (current == null) {
          current = tab[index++];           // <-- dup_x1 field-post-inc array index
        } else {
          int k = current.key;              // local 3
          current = current.next;
          action.accept(k);                 // the call inside the method
          if (map.modCount != expectedModCount) throw new IllegalStateException();
          return true;
        }
      }
    }
    return false;
  }

  public static void main(String[] a) {
    OsrDupX2 s = new OsrDupX2();
    s.map = new Map();
    s.map.table = new Node[16];
    for (int i = 0; i < 16; i += 2) s.map.table[i] = new Node(i);
    long sum = 0;
    final long[] acc = {0};
    IntConsumer c = (int k) -> acc[0] += k;
    for (int it = 0; it < 5_000_000; it++) {
      s.current = null; s.index = 0; s.fence = -1; s.expectedModCount = 0;
      acc[0] = 0;
      while (s.tryAdvance(c)) { }            // drive the spliterator to exhaustion each iter
      sum += acc[0];
      if ((it & 0xFFFFF) == 0) System.out.println("it=" + it + " acc=" + acc[0]);
    }
    System.out.println("FINAL sum=" + sum + " (expect " + (5_000_000L * (0+2+4+6+8+10+12+14)) + ")");
  }
}
