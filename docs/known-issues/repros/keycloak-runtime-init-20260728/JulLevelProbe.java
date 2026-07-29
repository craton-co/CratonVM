import java.util.logging.Level;
import java.util.logging.Logger;

public class JulLevelProbe {
    public static void main(String[] a) {
        Logger root = Logger.getLogger("");
        Logger l = Logger.getLogger("com.example.Foo");
        System.out.println("root.getLevel()=" + root.getLevel());
        System.out.println("foo.getLevel()=" + l.getLevel());
        System.out.println("foo.getParent()=" + (l.getParent() == null ? "null" : "'" + l.getParent().getName() + "'"));
        System.out.println("foo.isLoggable(FINEST)=" + l.isLoggable(Level.FINEST));
        System.out.println("foo.isLoggable(FINE)=" + l.isLoggable(Level.FINE));
        System.out.println("foo.isLoggable(INFO)=" + l.isLoggable(Level.INFO));
        System.out.println("foo.isLoggable(SEVERE)=" + l.isLoggable(Level.SEVERE));
        l.setLevel(Level.FINE);
        System.out.println("--- after foo.setLevel(FINE) ---");
        System.out.println("foo.isLoggable(FINE)=" + l.isLoggable(Level.FINE));
        System.out.println("foo.isLoggable(FINEST)=" + l.isLoggable(Level.FINEST));
        Logger child = Logger.getLogger("com.example.Foo.Bar");
        System.out.println("child.isLoggable(FINE)=" + child.isLoggable(Level.FINE));
        System.out.println("child.isLoggable(FINEST)=" + child.isLoggable(Level.FINEST));
        System.out.println("== DONE OK ==");
    }
}
