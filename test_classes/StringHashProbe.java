public class StringHashProbe {
    public static void main(String[] a) {
        String s1 = "java.lang.Object";
        String s2 = Object.class.getName();
        String s3 = new String(s1);
        System.out.println("s1.hashCode = " + s1.hashCode());
        System.out.println("s2.hashCode = " + s2.hashCode());
        System.out.println("s3.hashCode = " + s3.hashCode());
        System.out.println("s1.equals(s2) = " + s1.equals(s2));
        System.out.println("s1.equals(s3) = " + s1.equals(s3));
        System.out.println("s2.equals(s3) = " + s2.equals(s3));
        // expected:
        // 1063877011 across the board
    }
}
