public class ExceptionTest {
    public static void main(String[] args) {
        try {
            int result = 10 / 0;
        } catch (ArithmeticException e) {
            System.out.println("Caught: " + e.getMessage());
        } finally {
            System.out.println("Finally block executed");
        }

        try {
            String s = null;
            s.length();
        } catch (NullPointerException e) {
            System.out.println("Caught NPE");
        }
    }
}
