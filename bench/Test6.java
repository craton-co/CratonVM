import java.util.HashMap;
import java.util.Map;

public class Test6 {
    public static void main(String[] args) {
        System.out.println("test6 start");

        Map<String, Integer> map = new HashMap<>();
        System.out.println("map created");

        map.put("a", 1);
        map.put("b", 2);
        System.out.println("put done, size=" + map.size());

        Integer val = map.get("a");
        System.out.println("get(a)=" + val);

        // getOrDefault
        int count = map.getOrDefault("a", 0);
        System.out.println("getOrDefault(a)=" + count);

        map.put("a", count + 1);
        System.out.println("put(a, " + (count+1) + ")");

        System.out.println("final get(a)=" + map.get("a"));
        System.out.println("DONE");
    }
}
