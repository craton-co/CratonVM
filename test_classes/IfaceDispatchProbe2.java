import java.util.stream.Stream;
import java.util.Arrays;
import java.util.function.Consumer;

public class IfaceDispatchProbe2 {
    public static void main(String[] args) {
        // Replicates the kc26 failing pattern: Stream.forEachOrdered
        Stream<String> s = Arrays.asList("a", "b", "c").stream();
        s.forEachOrdered(new Consumer<String>() {
            @Override public void accept(String x) { System.out.println("got: " + x); }
        });
    }
}
