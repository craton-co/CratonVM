/**
 * SBR5 lane clload: one unsealed package split over two jars with different manifests. The Package a class reports
 * is the one the FIRST loaded class's jar defined. Class path: s1.jar:s2.jar (com.split.A in s1, com.split.B in s2).
 * Load order from the first argument: "BA" loads B then A.
 */
public class Sbr5ClloadSplit {
    static void p(String k, Object v) { System.out.println(k + ": " + v); }
    public static void main(String[] a) throws Exception {
        String order = a.length > 0 ? a[0] : "AB";
        ClassLoader l = ClassLoader.getSystemClassLoader();
        for (char c : order.toCharArray()) {
            Class<?> k = Class.forName("com.split." + c, false, l);
            p(c + " loc", k.getProtectionDomain().getCodeSource().getLocation().toString().replaceAll(".*/", ""));
            p(c + " pkg impl", k.getPackage().getImplementationVersion());
            p(c + " pkg sealed", k.getPackage().isSealed());
        }
        p("pkg same", Class.forName("com.split.A", false, l).getPackage() == Class.forName("com.split.B", false, l).getPackage());
        p("defined", l.getDefinedPackage("com.split").getImplementationVersion());
    }
}
