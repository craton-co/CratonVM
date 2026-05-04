import com.google.common.collect.ImmutableList;

public class Min1 {
    public static void main(String[] args) {
        ImmutableList<Integer> xs = ImmutableList.of(3, 1, 4, 1, 5, 9, 2, 6);
        int min = xs.stream().mapToInt(Integer::intValue).min().getAsInt();
        System.out.println("min=" + min);
        if (min != 1) throw new AssertionError("min wrong");
        System.out.println("Min1: PASS");
    }
}
