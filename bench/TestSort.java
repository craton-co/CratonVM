import java.util.*;

public class TestSort {
    public static void main(String[] args) {
        List<String> names = new ArrayList<>();
        names.add("Charlie");
        names.add("Alice");
        names.add("Bob");
        System.out.println("before: " + names.get(0) + "," + names.get(1) + "," + names.get(2));

        names.sort((a, b) -> a.compareTo(b));
        System.out.println("after: " + names.get(0) + "," + names.get(1) + "," + names.get(2));

        // Also test Collections.sort
        List<String> names2 = new ArrayList<>();
        names2.add("Zebra");
        names2.add("Apple");
        names2.add("Mango");
        Collections.sort(names2);
        System.out.println("sort2: " + names2.get(0) + "," + names2.get(1) + "," + names2.get(2));

        System.out.println("DONE");
    }
}
