import java.util.*;
public class AdFailFast {
  public static void main(String[] a) {
    t("add during iteration",      d -> d.add("z"));
    t("addFirst during iteration", d -> d.addFirst("z"));
    t("removeLast during iteration", d -> d.removeLast());
    t("clear during iteration",    d -> d.clear());
    // Control: the same shape on ArrayList, which is fail-fast on both VMs.
    List<String> l = new ArrayList<>(List.of("a","b","c"));
    String r;
    try { Iterator<String> it = l.iterator(); it.next(); l.add("z"); it.next(); r = "false"; }
    catch (ConcurrentModificationException e) { r = "true"; }
    catch (Throwable x) { r = x.getClass().getSimpleName(); }
    System.out.println("CONTROL ArrayList add | failFast=" + r);
  }
  interface M { void go(ArrayDeque<String> d); }
  static void t(String label, M m) {
    ArrayDeque<String> d = new ArrayDeque<>(List.of("a","b","c"));
    String r;
    try {
      Iterator<String> it = d.iterator();
      it.next();
      m.go(d);
      it.next();
      r = "false";
    } catch (ConcurrentModificationException e) { r = "true"; }
    catch (Throwable x) { r = x.getClass().getSimpleName(); }
    System.out.println("ArrayDeque " + label + " | failFast=" + r
        + " itrClass=" + new ArrayDeque<>(List.of("a")).iterator().getClass().getName());
  }
}
