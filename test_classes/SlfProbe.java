import org.slf4j.Logger;
import org.slf4j.LoggerFactory;
import org.slf4j.Marker;
import org.slf4j.MarkerFactory;
public class SlfProbe {
    public static void main(String[] args) {
        Logger lg = LoggerFactory.getLogger("test");
        System.out.println("Logger class: " + lg.getClass().getName());
        Marker m = MarkerFactory.getMarker("M");
        try {
            boolean b = lg.isErrorEnabled(m);
            System.out.println("isErrorEnabled(Marker)=" + b);
        } catch (Throwable t) {
            System.out.println("Caught: " + t);
        }
    }
}
