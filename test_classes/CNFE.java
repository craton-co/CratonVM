public class CNFE {
    public static void main(String[] args) {
        try {
            Class.forName("does.not.exist.Foo");
            System.out.println("FAIL -- should have thrown");
        } catch (ClassNotFoundException e) {
            System.out.println("OK caught CNF");
        } catch (Exception e) {
            System.out.println("caught other: " + e);
        }
    }
}
