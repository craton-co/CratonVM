import java.util.*;
public class TSProbe {
  public static void main(String[] a) {
    TreeSet<String> ts = new TreeSet<>();
    ts.add("b"); ts.add("a"); ts.add("c");
    System.out.println("before="+ts);
    Iterator<String> it = ts.iterator();
    while (it.hasNext()) {
      String s = it.next();
      if (s.equals("b")) { it.remove(); }
    }
    System.out.println("after="+ts);

    // also plain HashSet iterator remove
    HashSet<String> hs = new HashSet<>();
    hs.add("x"); hs.add("y");
    Iterator<String> hi = hs.iterator();
    hi.next(); hi.remove();
    System.out.println("hashset after="+hs);
  }
}
