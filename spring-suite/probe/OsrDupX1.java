// spring-bug-11 clean OSR repro: the exact `this.current = this.table[this.index++]`
// idiom from HashMap$KeySpliterator.tryAdvance (dup_x1 at the field-post-increment
// array index), driven hot so the loop OSR-compiles. No native HashMap, so no
// interpreter contamination. Compare the walked count to HotSpot.
public class OsrDupX1 {
  static final class Node { Node next; int v; Node(int v){this.v=v;} }
  Node[] table;
  Node current;
  int index;

  // mirrors tryAdvance's inner loop: advance `current` over the table.
  long walk() {
    long acc = 0;
    index = 0;
    current = null;
    // OSR target: this while-loop back-edge.
    while (true) {
      if (current == null) {
        if (index >= table.length) break;
        current = table[index++];   // aload_0; aload_0 getfield table; aload_0 dup getfield index; dup_x1; iconst_1; iadd; putfield index; aaload; putfield current
      } else {
        acc += current.v;
        current = current.next;
      }
    }
    return acc;
  }

  public static void main(String[] a) {
    OsrDupX1 s = new OsrDupX1();
    s.table = new Node[16];
    for (int i = 0; i < 16; i += 2) { Node n = new Node(i); n.next = new Node(i+100); s.table[i] = n; }
    long expectOne = s.walk();
    long sum = 0;
    for (int it = 0; it < 5_000_000; it++) {       // force the walk() loop to OSR-compile
      sum += s.walk();
      if ((it & 0xFFFFF) == 0) System.out.println("it=" + it + " one=" + s.walk());
    }
    System.out.println("FINAL sum=" + sum + " expectOne=" + expectOne);
  }
}
