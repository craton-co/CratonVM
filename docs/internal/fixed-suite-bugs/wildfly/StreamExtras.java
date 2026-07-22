import java.util.stream.Stream;
import java.util.stream.Collectors;
public class StreamExtras {
    public static void main(String[] a) {
        System.out.println("peek:");
        long n = Stream.of("a","b","c").peek(s -> System.out.println("  pk:"+s)).count();
        System.out.println("  count="+n);
        System.out.println("takeWhile:");
        Stream.of(1,2,3,4).takeWhile(i -> i<3).forEach(i -> System.out.println("  tw:"+i));
        System.out.println("forEachOrdered:");
        Stream.of("x","y").forEachOrdered(s -> System.out.println("  ord:"+s));
        System.out.println("done");
    }
}
