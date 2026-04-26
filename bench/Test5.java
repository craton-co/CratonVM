import java.util.ArrayList;
import java.util.List;

public class Test5 {
    public static void main(String[] args) {
        System.out.println("test5 start");

        List<Integer> nums = new ArrayList<>();
        nums.add(10);
        nums.add(20);
        nums.add(30);
        System.out.println("list ready");

        // Enhanced for-each (uses Iterator)
        int sum = 0;
        for (int x : nums) {
            System.out.println("  item=" + x);
            sum += x;
        }
        System.out.println("sum=" + sum);
        System.out.println("DONE");
    }
}
