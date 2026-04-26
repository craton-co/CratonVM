public class TestInteger2 {
    static Integer box(int x) {
        return Integer.valueOf(x);
    }
    public static void main(String[] args) {
        System.out.println("Start");
        Integer i = box(42);
        System.out.println("boxed: " + i);
        System.out.println("Done");
    }
}
