import java.util.*;
import java.util.stream.*;
public class StreamProbe {
  public static void main(String[] a) {
    OptionalInt x = IntStream.of(3,1,2).findFirst();
    System.out.println("IntStream.findFirst=" + x.getAsInt());
    List<Integer> l = Stream.of(1,2,3).collect(Collectors.toList());
    System.out.println("Collectors.toList=" + l);
    Optional<Integer> m = Stream.of(5,6).reduce(Integer::sum);
    System.out.println("reduce=" + m.get());
    System.out.println("PROBE_OK");
  }
}
