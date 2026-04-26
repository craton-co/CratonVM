import java.util.ArrayList;

public class TestToString {
    public static void main(String[] args) {
        // Test 1: ArrayList toString via string concatenation
        ArrayList<Integer> list = new ArrayList<>();
        list.add(1); list.add(2); list.add(3);
        System.out.println("list=" + list);

        // Test 2: Integer in concat (should show value, not hash)
        Integer x = 42;
        System.out.println("int=" + x);

        // Test 3: Boolean
        Boolean b = true;
        System.out.println("bool=" + b);

        // Test 4: Null
        Object n = null;
        System.out.println("null=" + n);

        System.out.println("DONE");
    }
}
