// Test fixture for reader::class_reader tests (constant-pool Utf8 interning).
// Compiled to HelloWorld.class (Java 8 / major 52) and committed; the test
// `include_bytes!`es the .class at compile time.
public class HelloWorld {
    public static void main(String[] args) {
        System.out.println("Hello, World!");
    }
}
