import java.util.*;
import java.util.stream.*;

public class streams_probe {
  public static void main(String[] a) {
    long c = Arrays.asList(1,2,3,4,5).stream().filter(x -> x > 2).count();
    System.out.println("count=" + c);
    Arrays.asList(1,2,3).stream().forEachOrdered(System.out::println);
  }
}
