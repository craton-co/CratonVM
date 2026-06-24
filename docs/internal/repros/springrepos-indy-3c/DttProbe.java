import org.codehaus.groovy.runtime.typehandling.DefaultTypeTransformation;
import java.util.ArrayList;
public class DttProbe {
  public static void main(String[] a) {
    System.out.println("castToBoolean(Boolean.FALSE)=" + DefaultTypeTransformation.castToBoolean(Boolean.FALSE) + " (expect false)");
    System.out.println("castToBoolean(Boolean.TRUE)=" + DefaultTypeTransformation.castToBoolean(Boolean.TRUE) + " (expect true)");
    System.out.println("castToBoolean('')=" + DefaultTypeTransformation.castToBoolean("") + " (expect false)");
    System.out.println("castToBoolean('x')=" + DefaultTypeTransformation.castToBoolean("x") + " (expect true)");
    System.out.println("castToBoolean(null)=" + DefaultTypeTransformation.castToBoolean(null) + " (expect false)");
    System.out.println("castToBoolean(emptyList)=" + DefaultTypeTransformation.castToBoolean(new ArrayList<>()) + " (expect false)");
    System.out.println("castToBoolean(Integer 0)=" + DefaultTypeTransformation.castToBoolean(Integer.valueOf(0)) + " (expect false)");
    System.out.println("DTTPROBE_DONE");
  }
}
