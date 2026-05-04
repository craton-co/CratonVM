public class ConsoleProbe {
    public static void main(String[] a) {
        java.io.Console c = System.console();
        System.out.println("console=" + c);
        System.out.println("OK");
    }
}
