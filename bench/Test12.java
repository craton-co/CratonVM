import java.util.HashMap;
import java.util.Map;

public class Test12 {
    public static void main(String[] args) {
        System.out.println("start");
        Map<String, Integer> wc = new HashMap<>();
        wc.put("a", 3);
        System.out.println("map ready");

        Integer val = wc.get("a");
        System.out.println("val=" + val);

        // Autoboxed comparison
        System.out.println("comparing");
        if (val == 3) {
            System.out.println("PASS");
        } else {
            System.out.println("FAIL");
        }
        System.out.println("DONE");
    }
}
