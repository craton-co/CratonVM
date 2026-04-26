public class TestExceptionVerify {
    public static void main(String[] args) {
        try {
            throw new java.io.IOException("test");
        } catch (java.io.IOException e) {
            System.out.println("caught IOException: " + e.getMessage());
        } catch (Throwable t) {
            System.out.println("caught Throwable");
        }
    }
}
