public class Test7 {
    public static void main(String[] args) {
        Integer i = Integer.valueOf(42);
        String s = i.toString();
        System.out.println("Integer.toString()=" + s);

        // Also test via String.valueOf
        String s2 = String.valueOf(42);
        System.out.println("String.valueOf(42)=" + s2);

        // Test string concat with int primitive
        int x = 99;
        System.out.println("x=" + x);

        // Test string concat with Integer object
        Integer y = Integer.valueOf(77);
        System.out.println("y=" + y);

        System.out.println("DONE");
    }
}
