import java.util.*;
public class IndexOfProbe {
  static class Named {
    final String n; Named(String n){this.n=n;}
    // asymmetric-ish: equality by name only, like Spring's PropertySource
    public boolean equals(Object o){ return o instanceof Named && ((Named)o).n.equals(n); }
    public int hashCode(){ return n.hashCode(); }
    public String toString(){ return "Named("+n+")"; }
  }
  static class Sub extends Named { Sub(String n){super(n);} }
  public static void main(String[] a) {
    List<Named> l = new ArrayList<>(List.of(new Named("a"), new Sub("b"), new Named("c")));
    System.out.println("indexOf(new Named(b)) = " + l.indexOf(new Named("b")));
    System.out.println("indexOf(new Sub(c))   = " + l.indexOf(new Sub("c")));
    System.out.println("contains(new Named(a))= " + l.contains(new Named("a")));
    System.out.println("lastIndexOf(Named(c)) = " + l.lastIndexOf(new Named("c")));
  }
}
