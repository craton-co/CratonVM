import java.util.*;

/** Is `System.getProperties()`'s REAL `map` field populated?
 *
 *  Every row here is answered by JDK bytecode that reads `this.map` when the
 *  Properties natives are yielded, so a VM whose system Properties keeps its
 *  entries only in a side table reads EMPTY on all of them while its native
 *  path still answers correctly. That is the split this probe exists to see:
 *  a null map is loud (NPE), an empty one is silent.
 *
 *  Nothing here prints a property VALUE or a count that the two VMs may choose
 *  independently -- the set of system properties differs between them by
 *  construction. It asks only for shape: more than ten entries, a known key
 *  present, and the clone agreeing with its source.
 */
public class SysPropsRealMapProbe {
    static int rows = 0;
    static void p(String tag, Object v) { System.out.println(++rows + " " + tag + " |" + v + "|"); }
    public static void main(String[] a) {
        Properties sys = System.getProperties();
        p("sys size > 10", sys.size() > 10);
        p("sys java.version present", sys.getProperty("java.version") != null);
        p("sys keySet size > 10", sys.keySet().size() > 10);
        p("sys stringPropertyNames > 10", sys.stringPropertyNames().size() > 10);

        Properties clone = (Properties) sys.clone();
        p("clone size > 10", clone.size() > 10);
        p("clone java.version present", clone.getProperty("java.version") != null);
        p("clone size equals sys size", clone.size() == sys.size());

        // A write through the system view must be visible to a later read, and
        // must survive the next getProperties() call -- the resync path.
        System.setProperty("probe.written", "yes");
        p("set then get", System.getProperties().getProperty("probe.written"));
        p("set then get twice", System.getProperties().getProperty("probe.written"));
        System.clearProperty("probe.written");
        p("cleared is gone", System.getProperties().getProperty("probe.written"));
        System.out.println("DONE SysPropsRealMapProbe");
    }
}
