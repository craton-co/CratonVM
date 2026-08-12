/**
 * Target for {@code ForNameCacheProbe}. Compiled into a directory that is NOT on
 * the application classpath, so only a {@code URLClassLoader} pointed at that
 * directory can find it and the application loader never defines a copy.
 */
public class ForNameCacheProbeTarget {
    public static String tag() {
        return "ForNameCacheProbeTarget-ok";
    }
}
