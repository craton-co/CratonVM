import java.util.Arrays;

public class Test13 {
    public static void main(String[] args) {
        System.out.println("start");
        int[] nums = {1, 2, 3, 4, 5, 6};
        System.out.println("array ready");

        long count = Arrays.stream(nums)
            .filter(n -> n % 2 == 0)
            .count();
        System.out.println("even count=" + count);
        System.out.println("DONE");
    }
}
