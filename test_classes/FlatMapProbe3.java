import java.util.Arrays;
import java.util.List;
import java.util.stream.Stream;
import java.util.stream.Collectors;

public class FlatMapProbe3 {
    public static void main(String[] args) {
        // 1D String[]
        String[] s = new String[] { "a", "b", "c" };
        List<String> l1 = Arrays.stream(s).collect(Collectors.toList());
        System.out.println("1D-String[]=" + l1);

        // 1D Integer[]
        Integer[] ints = new Integer[] { 1, 2, 3 };
        List<Integer> l2 = Arrays.stream(ints).collect(Collectors.toList());
        System.out.println("1D-Integer[]=" + l2);

        // 2D
        String[][] arr2 = new String[][] { {"a"}, {"b"} };
        long n = Arrays.stream(arr2).count();
        System.out.println("2D-count=" + n);

        // Object[]
        Object[] o = new Object[] { "x", "y" };
        long n2 = Arrays.stream(o).count();
        System.out.println("Object[]-count=" + n2);

        // Stream.of on String[][]
        long n3 = Stream.of(arr2).count();
        System.out.println("Stream.of(2D)-count=" + n3);
    }
}
