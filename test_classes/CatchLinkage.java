public class CatchLinkage {
    static class Outer {
        static {
            try { Class.forName("really.missing"); }
            catch (Exception e) { System.out.println("swallowed: " + e.getClass().getName()); }
        }
    }
    public static void main(String[] args) {
        new Outer();
        System.out.println("OK");
    }
}
