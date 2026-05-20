public class StringTest {
    public static void main(String[] args) {
        String s = "Hello, CratonVM!";
        System.out.println("Length: " + s.length());
        System.out.println("Upper: " + s.toUpperCase());
        System.out.println("Sub: " + s.substring(0, 5));
        System.out.println("Contains 'JVM': " + s.contains("JVM"));
        System.out.println("Replace: " + s.replace("Rust", "Fast"));
    }
}
