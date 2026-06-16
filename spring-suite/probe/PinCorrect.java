// spring-bug-10 correctness check: exercise oop operands held across GC-capable
// calls (the make/check register-invisibility pattern) plus heavy allocation,
// and print a deterministic checksum. Baseline (no shadow) and SHADOW+PIN must
// produce IDENTICAL output — proving the pin-aware reload's skip-value-restore
// does not silently corrupt live home values.
public class PinCorrect {
  static final class Node { Node l, r; int v; Node(Node l, Node r, int v){this.l=l;this.r=r;this.v=v;} }
  static Node make(int d, int v) {
    if (d == 0) return new Node(null, null, v);
    // l and r are oops live on the operand stack across the recursive calls.
    Node l = make(d - 1, v * 2);
    Node r = make(d - 1, v * 2 + 1);
    return new Node(l, r, v);
  }
  static long check(Node n) {
    if (n == null) return 0;
    return n.v + check(n.l) + check(n.r);  // n live across the recursive call
  }
  public static void main(String[] a) {
    long sum = 0;
    for (int iter = 0; iter < 2000; iter++) {
      Node t = make(10, iter);     // ~1023 nodes, oops across calls
      sum += check(t);
      // churn the young gen so GC runs while make/check oops are live
      java.util.ArrayList<int[]> g = new java.util.ArrayList<>();
      for (int k = 0; k < 200; k++) g.add(new int[8]);
    }
    System.out.println("CHECKSUM=" + sum);
  }
}
