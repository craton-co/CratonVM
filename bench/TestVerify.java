public class TestVerify {
    public static void main(String[] args) {
        try {
            String s = "hello";
            CharSequence cs = s;  // String → CharSequence
            System.out.println(cs.length());
        } catch (Throwable t) {   // Should catch any exception
            System.out.println("caught: " + t);
        }
    }
}
