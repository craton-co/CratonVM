import java.util.*;
import java.util.stream.*;
import java.util.function.Function;

public class WeakRefProbe {
    public static void main(String[] a) throws Exception {
        // T1: just stream().findFirst()
        Optional<String> t1 = Arrays.asList("hello", "world").stream().findFirst();
        System.out.println("T1 findFirst = " + t1.orElse("EMPTY"));

        // T2: map().findFirst()
        Optional<String> t2 = Arrays.asList("hello", "world").stream()
            .map(String::toUpperCase)
            .findFirst();
        System.out.println("T2 map.findFirst = " + t2.orElse("EMPTY"));

        // T3: flatMap with Stream.of
        Optional<String> t3 = Arrays.asList("hello", "world").stream()
            .flatMap(s -> Stream.of(s))
            .findFirst();
        System.out.println("T3 flatMap.findFirst = " + t3.orElse("EMPTY"));

        // T4: map + flatMap
        Optional<String> t4 = Arrays.asList("hello", "world").stream()
            .map(String::toUpperCase)
            .flatMap(s -> Stream.of(s))
            .findFirst();
        System.out.println("T4 map+flatMap.findFirst = " + t4.orElse("EMPTY"));

        // T5: map + flatMap with Optional::stream
        Function<String, Optional<String>> opt = s -> Optional.of(s.toUpperCase());
        Optional<String> t5 = Arrays.asList("hello", "world").stream()
            .map(opt)
            .flatMap(Optional::stream)
            .findFirst();
        System.out.println("T5 map+flatMap(Opt::stream).findFirst = " + t5.orElse("EMPTY"));

        // T6: Optional.of(X).stream().findFirst()
        Optional<String> t6 = Optional.of("X").stream().findFirst();
        System.out.println("T6 Opt.stream.findFirst = " + t6.orElse("EMPTY"));

        // T7: directly flatMap(Optional::stream) from Optional-typed stream
        Optional<String> t7 = Stream.of(Optional.of("HELLO"))
            .flatMap(Optional::stream)
            .findFirst();
        System.out.println("T7 Stream<Optional>.flatMap.findFirst = " + t7.orElse("EMPTY"));
    }
}
