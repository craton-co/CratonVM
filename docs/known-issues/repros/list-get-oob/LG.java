import java.util.*;

/**
 * `List.get` out of range must throw IndexOutOfBoundsException with HotSpot's
 * message, not a bare ArrayIndexOutOfBoundsException.
 *
 * Found from Spring's CompileWithForkedClassLoaderExtension.runTest:140,
 *   throw summary.getFailures().get(0).getException();
 * which surfaced as `java.lang.ArrayIndexOutOfBoundsException: null` on
 * CratonVM and masked whatever the nested test had actually failed on.
 */
public class LG {
    static void t(String name, Runnable r) {
        try {
            r.run();
            System.out.println(name + " NOTHROW");
        } catch (Throwable e) {
            System.out.println(name + " " + e.getClass().getName() + ": " + e.getMessage());
        }
    }
    public static void main(String[] a) {
        t("ArrayList.get(0) empty      ", () -> new ArrayList<String>().get(0));
        t("ArrayList.get(5) size3      ", () -> {
            List<String> l = new ArrayList<>(List.of("a", "b", "c")); l.get(5);
        });
        t("ArrayList.get(-1)           ", () -> new ArrayList<>(List.of("a")).get(-1));
        t("List.of().get(0)            ", () -> List.of().get(0));
        t("Arrays.asList().get(0)      ", () -> Arrays.asList().get(0));
        t("LinkedList.get(0) empty     ", () -> new LinkedList<String>().get(0));
        t("subList/get                 ", () -> new ArrayList<>(List.of("a","b")).subList(0,1).get(3));
        t("raw array [5] of len 3      ", () -> { int[] x = new int[3]; int y = x[5]; });
        t("Collections.emptyList()     ", () -> Collections.emptyList().get(0));
        // The exact shape Spring hits.
        t("size>0 but list empty       ", () -> {
            List<String> l = new ArrayList<>();
            if (l.size() == 0) { l.get(0); }
        });
    }
}
