import java.util.function.BiFunction;

public class TestCompare2 {
    public static void main(String[] args) {
        // Simple BiFunction that returns a string of the args
        BiFunction<String, String, String> show = (a, b) -> "a=" + a + " b=" + b;
        System.out.println(show.apply("X", "Y"));

        // Comparator as lambda
        java.util.Comparator<String> cmp = (a, b) -> {
            System.out.println("  compare: a=" + a + " b=" + b);
            return a.compareTo(b);
        };
        int r = cmp.compare("Alice", "Bob");
        System.out.println("result=" + r);
        System.out.println("DONE");
    }
}
