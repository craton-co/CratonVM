public class ExceptionTest {
    public static void main(String[] args) {
        // Test 1: Catch NPE
        try {
            String s = null;
            int len = s.length();
            System.out.println("ERROR: should have thrown NPE");
        } catch (NullPointerException e) {
            System.out.println("Caught NPE: OK");
        }

        // Test 2: Catch ArithmeticException
        try {
            int x = 10 / 0;
            System.out.println("ERROR: should have thrown ArithmeticException");
        } catch (ArithmeticException e) {
            System.out.println("Caught ArithmeticException: OK");
        }

        // Test 3: Catch ArrayIndexOutOfBoundsException
        try {
            int[] arr = new int[3];
            int v = arr[10];
            System.out.println("ERROR: should have thrown AIOOBE");
        } catch (ArrayIndexOutOfBoundsException e) {
            System.out.println("Caught AIOOBE: OK");
        }

        // Test 4: Finally block
        try {
            System.out.println("In try block");
        } finally {
            System.out.println("In finally block: OK");
        }

        System.out.println("All exception tests passed!");
    }
}
