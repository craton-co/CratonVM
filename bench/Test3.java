public class Test3 {
    public static void main(String[] args) {
        System.out.println("test3 start");

        // Test String.equals
        String a = "olleh";
        String b = new StringBuilder("hello").reverse().toString();
        System.out.println("a=" + a + " b=" + b);
        boolean eq = a.equals(b);
        System.out.println("equals: " + eq);

        System.out.println("DONE");
    }
}
