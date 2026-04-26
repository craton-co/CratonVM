import java.util.ArrayList;
import java.util.List;

public class Test4 {
    public static void main(String[] args) {
        System.out.println("test4 start");

        List<Integer> nums = new ArrayList<>();
        System.out.println("created list");
        nums.add(1);
        nums.add(2);
        nums.add(3);
        System.out.println("added elements, size=" + nums.size());

        // Test simple iteration with index
        int sum = 0;
        for (int i = 0; i < nums.size(); i++) {
            sum += nums.get(i);
        }
        System.out.println("sum=" + sum);

        System.out.println("DONE");
    }
}
