import java.util.*;
/** Which layer of ArrayList.get's bounds check diverges. */
public class LG2 {
    static void t(String n, Runnable r) {
        try { r.run(); System.out.println(n + " NOTHROW"); }
        catch (Throwable e) { System.out.println(n + " " + e.getClass().getName() + ": " + e.getMessage()); }
    }
    public static void main(String[] a) {
        t("Objects.checkIndex(0,0)     ", () -> Objects.checkIndex(0, 0));
        t("Objects.checkIndex(5,3)     ", () -> Objects.checkIndex(5, 3));
        t("Objects.checkFromToIndex    ", () -> Objects.checkFromToIndex(0, 5, 3));
        t("Objects.checkFromIndexSize  ", () -> Objects.checkFromIndexSize(0, 5, 3));
        t("raw int[3][5]               ", () -> { int[] x = new int[3]; int y = x[5]; });
        t("raw Object[3][5]            ", () -> { Object[] x = new Object[3]; Object y = x[5]; });
        t("ArrayList.get(0) empty      ", () -> new ArrayList<String>().get(0));
        t("String.charAt(5) of len 3   ", () -> "abc".charAt(5));
    }
}
