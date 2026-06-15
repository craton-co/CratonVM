import java.util.stream.*;
public class StreamClass {
  public static void main(String[] a){
    System.out.println("of      = " + IntStream.of(3,1,2).getClass().getName());
    System.out.println("range   = " + IntStream.range(0,5).getClass().getName());
    System.out.println("rangeF  = " + IntStream.range(0,5).filter(i->i>1).getClass().getName());
    System.out.println("mapToInt= " + Stream.of(1,2).mapToInt(Integer::intValue).getClass().getName());
    System.out.println("arrays  = " + java.util.Arrays.stream(new int[]{1,2}).getClass().getName());
    // does findFirst have Code on each receiver's class hierarchy?
    System.out.println("DONE");
  }
}
