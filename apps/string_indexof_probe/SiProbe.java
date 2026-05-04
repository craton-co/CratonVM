public class SiProbe {
    public static void main(String[] a) {
        String s = "hello world hello";
        System.out.println("idx=" + s.indexOf("world"));    // expect 6
        System.out.println("idx2=" + s.indexOf("hello", 3)); // expect 12
        System.out.println("OK");
    }
}
