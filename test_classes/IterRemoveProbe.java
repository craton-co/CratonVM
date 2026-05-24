import java.util.*;

public class IterRemoveProbe {
    public static void main(String[] args) {
        HashSet<String> s = new HashSet<>();
        s.add("a"); s.add("b"); s.add("c");
        Iterator<String> it = s.iterator();
        System.out.println("iter class: " + it.getClass().getName());
        while (it.hasNext()) {
            String x = it.next();
            if (x.equals("b")) {
                it.remove();
                System.out.println("removed: " + x);
            }
        }
        System.out.println("after: " + s.size());
    }
}
