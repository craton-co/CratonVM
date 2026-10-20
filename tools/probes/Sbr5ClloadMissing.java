/**
 * SBR5 lane clload: failure shapes of application-loader lookups. msjar.jar has ms.Sub extends ms.Gone (Gone is not
 * in the jar), ms.Ok, and ms.Iface2 implements ms.GoneI. Each lookup is done twice (the second must be the same).
 */
public class Sbr5ClloadMissing {
    static void p(String k, Object v) { System.out.println(k + ": " + v); }
    static void t(String cn) {
        ClassLoader l = ClassLoader.getSystemClassLoader();
        for (int i = 0; i < 2; i++) {
            try { p(cn + " #" + i, Class.forName(cn, false, l)); }
            catch (Throwable x) {
                Throwable c = x.getCause();
                p(cn + " #" + i, x.getClass().getName() + ": " + x.getMessage()
                        + (c == null ? "" : " | cause " + c.getClass().getName() + ": " + c.getMessage()));
            }
        }
        try { l.loadClass(cn); p(cn + " loadClass", "ok"); }
        catch (Throwable x) { p(cn + " loadClass", x.getClass().getName() + ": " + x.getMessage()); }
    }
    public static void main(String[] a) {
        t("ms.Ok"); t("ms.Sub"); t("ms.Iface2"); t("ms.Absent"); t("nopkg.Absent"); t("ms.Sub");
    }
}
