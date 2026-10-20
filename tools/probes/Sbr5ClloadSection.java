/**
 * SBR5 lane clload: a jar whose manifest has an UNSEALED per-package section (Name: com/se/ with its own
 * Implementation-Version / -Title). The Package must carry the section's values over the main section's.
 * Setup: tools/probes/sbr5-clload-section-setup.sh (writes sect/sect.jar and compiles this class into sect/).
 */
public class Sbr5ClloadSection {
    public static void main(String[] a) throws Exception {
        ClassLoader l = ClassLoader.getSystemClassLoader();
        Class<?> p = Class.forName("com.se.P", false, l);
        Package k = p.getPackage();
        System.out.println("impl=" + k.getImplementationVersion() + " title=" + k.getImplementationTitle()
                + " spec=" + k.getSpecificationTitle() + " sealed=" + k.isSealed());
        Package q = Class.forName("com.se.Q", false, l).getPackage();
        System.out.println("same=" + (q == k));
    }
}
