import java.util.ArrayList;
import java.util.List;

public class Test10 {
    public static void main(String[] args) {
        System.out.println("start");

        // Test 3: List sum with for-each
        List<Integer> nums = new ArrayList<>();
        nums.add(1); nums.add(2); nums.add(3); nums.add(4); nums.add(5);
        System.out.println("list created");

        int sum = 0;
        for (int x : nums) {
            sum += x;
        }
        System.out.println("sum=" + sum);

        if (sum == 15) {
            System.out.println("PASS: sumList = 15");
        }

        // Test 4: HashMap
        System.out.println("starting hashmap test");
        java.util.Map<String, Integer> map = new java.util.HashMap<>();
        String[] words = {"a", "b", "a", "c", "b", "a"};
        for (String w : words) {
            map.put(w, map.getOrDefault(w, 0) + 1);
        }
        System.out.println("wordcount done, a=" + map.get("a"));
        System.out.println("DONE");
    }
}
