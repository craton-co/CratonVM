/** Class.forName(String) is caller-sensitive: the caller class's loader must be the application loader. Both modes against HotSpot. */
public class ClassForNameCaller {
    public static void main(String[] a) throws Exception {
        for (String n : new String[]{"java.util.ArrayList", "org.junit.platform.launcher.core.DefaultLauncher", "ClassForNameCaller", "ClassForNameCallerOther"}) {
            try { System.out.println(n + " -> " + Class.forName(n)); } catch (Throwable t) { System.out.println(n + " !! " + t); }
        }
    }
}
class ClassForNameCallerOther {}
