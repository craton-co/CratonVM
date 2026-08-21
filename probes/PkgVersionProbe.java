public class PkgVersionProbe {
    static void show(String label, Class<?> c) {
        Package p = c.getPackage();
        System.out.println(label + " pkgNull=" + (p == null)
            + " name=" + (p == null ? "-" : p.getName())
            + " implTitle=" + (p == null ? "-" : String.valueOf(p.getImplementationTitle()))
            + " implVersion=" + (p == null ? "-" : String.valueOf(p.getImplementationVersion()))
            + " implVendor=" + (p == null ? "-" : String.valueOf(p.getImplementationVendor()))
            + " specVersion=" + (p == null ? "-" : String.valueOf(p.getSpecificationVersion()))
            + " sealed=" + (p == null ? "-" : String.valueOf(p.isSealed())));
    }
    public static void main(String[] args) throws Exception {
        show("self(dir)      ", PkgVersionProbe.class);
        show("java.lang.String", String.class);
        for (String cn : args) {
            try { show(cn, Class.forName(cn)); }
            catch (Throwable t) { System.out.println(cn + " -> " + t); }
        }
    }
}
