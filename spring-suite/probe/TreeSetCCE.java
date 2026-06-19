import java.io.InputStream;
import java.io.OutputStream;
import java.util.Set;
import java.util.TreeSet;

// Mirrors Spring's ValueCodeGeneratorDelegates.SetDelegate.orderForCodeConsistency:
// build a TreeSet from a Set whose elements are NOT Comparable (java.lang.Class).
// Real JDK throws ClassCastException (caught); CratonVM previously threw
// NoSuchMethodError (NOT caught) -> escaped -> JUnit "multiple times".
public class TreeSetCCE {
    public static void main(String[] args) {
        Set<Class<?>> set = Set.of(InputStream.class, OutputStream.class);
        try {
            Set<?> ordered = new TreeSet<Object>(set);
            System.out.println("NO-THROW size=" + ordered.size() + " (UNEXPECTED on JDK/CV-fixed only if 1 elem)");
        } catch (ClassCastException ex) {
            System.out.println("OK caught ClassCastException: " + ex.getMessage());
        } catch (Throwable t) {
            System.out.println("BAD caught " + t.getClass().getName() + ": " + t.getMessage());
        }
    }
}
