import java.util.*;
import java.util.concurrent.CopyOnWriteArrayList;
public class CowProbe {
  static class Named {
    final String n; Named(String n){this.n=n;}
    public boolean equals(Object o){ return o instanceof Named && ((Named)o).n.equals(n); }
    public int hashCode(){ return n.hashCode(); }
    public String toString(){ return "Named("+n+")"; }
  }
  public static void main(String[] a) {
    CopyOnWriteArrayList<Named> l = new CopyOnWriteArrayList<>();
    l.add(new Named("a")); l.add(new Named("b")); l.add(new Named("c"));
    System.out.println("size            = " + l.size());
    System.out.println("indexOf(b)      = " + l.indexOf(new Named("b")));
    System.out.println("contains(c)     = " + l.contains(new Named("c")));
    System.out.println("lastIndexOf(a)  = " + l.lastIndexOf(new Named("a")));
    System.out.println("remove(Named(a))= " + l.remove(new Named("a")) + " size=" + l.size());
  }
}
