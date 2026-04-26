public class TestInteger {
    public static void main(String[] args) {
        System.out.println("A");
        int x = 42;
        System.out.println("B: " + x);
        System.out.println("C: creating Integer");
        Integer i = Integer.valueOf(x);
        System.out.println("D: " + i);
    }
}
