public class IsArrayTest {
    public static void main(String[] args) {
        System.out.println("int[].isArray=" + int[].class.isArray());
        System.out.println("String[].isArray=" + String[].class.isArray());
        System.out.println("String.isArray=" + String.class.isArray());
        System.out.println("int.isArray=" + int.class.isArray());
        System.out.println("OK");
    }
}
