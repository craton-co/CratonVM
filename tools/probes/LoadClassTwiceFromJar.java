/** ClassLoader.loadClass of several classes of one jar (run with a jar on -cp, e.g. junit-platform-launcher): every row a class. Both modes against HotSpot. */
public class LoadClassTwiceFromJar {
    static String ld(ClassLoader l, String n) { try { return String.valueOf(l.loadClass(n)); } catch (Throwable x) { return "!" + x; } }
    public static void main(String[] a) {
        ClassLoader l = LoadClassTwiceFromJar.class.getClassLoader();
        String p = "org.junit.platform.launcher.core.";
        for (String c : new String[]{"DefaultLauncher", "LauncherFactory", "LauncherConfig", "EngineDiscoveryOrchestrator"}) System.out.println(c + " " + ld(l, p + c));
        System.out.println("other pkg " + ld(l, "org.junit.platform.launcher.TestPlan"));
    }
}
