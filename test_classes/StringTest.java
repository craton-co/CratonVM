public class StringTest {
    public static void main(String[] args) {
        String hello = "Hello";
        String world = "World";
        String combined = hello + ", " + world + "!";
        System.out.println(combined);

        System.out.println(combined.length());
        System.out.println(combined.charAt(0));
        System.out.println(combined.substring(7));
        System.out.println(combined.toUpperCase());
        System.out.println(combined.contains("World"));
        System.out.println(combined.indexOf("World"));

        System.out.println("All string tests passed!");
    }
}
