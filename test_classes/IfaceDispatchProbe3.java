import java.util.stream.Stream;
import java.util.Arrays;
import java.util.function.Consumer;

public class IfaceDispatchProbe3 {
    public static void main(String[] args) {
        Stream<String> s = Arrays.asList("a", "b", "c").stream();
        System.out.println("Got stream class: " + s.getClass().getName());
        // Now call forEachOrdered
        try {
            s.forEachOrdered(new Consumer<String>() {
                @Override public void accept(String x) { System.out.println("got: " + x); }
            });
        } catch (Throwable t) {
            System.out.println("Caught: " + t);
        }
    }
}
