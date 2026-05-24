import java.util.Arrays;
import java.util.List;
import java.util.stream.Stream;
import java.util.stream.Collectors;

public class FlatMapProbe4 {
    public static void main(String[] args) {
        String[] s = new String[] { "a", "b", "c" };
        Stream<String> st = Arrays.stream(s);
        System.out.println("stream-class=" + st.getClass().getName());
        Object[] arr = st.toArray();
        System.out.println("toArray-len=" + arr.length);

        // again, fresh stream
        Stream<String> st2 = Arrays.stream(s);
        long c = st2.count();
        System.out.println("count=" + c);

        // forEach
        System.out.println("--forEach--");
        Arrays.stream(s).forEach(x -> System.out.println("  el=" + x));
    }
}
