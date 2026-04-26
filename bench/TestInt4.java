public class TestInt4 {
    public static void main(String[] args) {
        System.out.println("A: parseInt");
        int x = Integer.parseInt("42");
        System.out.println("B: " + x);
        System.out.println("C: valueOf");
        Integer i = Integer.valueOf(x);
        System.out.println("D: done");
    }
}
