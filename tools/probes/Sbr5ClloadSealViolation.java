/**
 * SBR5 lane clload: sealing violation across jars. com.sbr2w9pkg is Sealed in pkg.jar (first on the class path);
 * other.jar carries com.sbr2w9pkg.D. HotSpot: loading D throws SecurityException "sealing violation". Needs both jars.
 */
public class Sbr5ClloadSealViolation {
    public static void main(String[] a) throws Exception {
        ClassLoader l = ClassLoader.getSystemClassLoader();
        System.out.println("A " + Class.forName("com.sbr2w9pkg.A", false, l).getPackage().isSealed());
        try { System.out.println("D " + Class.forName("com.sbr2w9pkg.D", false, l)); }
        catch (Throwable t) { System.out.println("D THROWS " + t.getClass().getName() + ": " + t.getMessage()); }
        try { System.out.println("D again " + l.loadClass("com.sbr2w9pkg.D")); }
        catch (Throwable t) { System.out.println("D again THROWS " + t.getClass().getName() + ": " + t.getMessage()); }
        System.out.println("def " + (l.getDefinedPackage("com.sbr2w9pkg") != null));
    }
}
