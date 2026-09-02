import java.util.*;
public class AdDispatch {
  public static void main(String[] x) {
    ArrayDeque<String> concrete = new ArrayDeque<>();
    concrete.add("a");
    System.out.println("concrete ArrayDeque.add | size=" + concrete.size() + " iter=" + n(concrete) + " peek=" + concrete.peekFirst());
    Deque<String> viaDeque = new ArrayDeque<>();
    viaDeque.add("a");
    System.out.println("Deque-typed .add       | size=" + viaDeque.size() + " iter=" + n(viaDeque) + " peek=" + viaDeque.peekFirst());
    Collection<String> viaColl = new ArrayDeque<>();
    viaColl.add("a");
    System.out.println("Collection-typed .add  | size=" + viaColl.size() + " iter=" + n(viaColl));
    ArrayDeque<String> viaAddLast = new ArrayDeque<>();
    viaAddLast.addLast("a");
    System.out.println("addLast                | size=" + viaAddLast.size() + " iter=" + n(viaAddLast));
  }
  static int n(Collection<?> c) { int k=0; for (Object o : c) k++; return k; }
}
