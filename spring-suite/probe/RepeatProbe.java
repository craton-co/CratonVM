import java.lang.annotation.*;
import java.lang.reflect.*;
public class RepeatProbe {
  @Retention(RetentionPolicy.RUNTIME) @Repeatable(Container.class)
  @interface R { String value(); }
  @Retention(RetentionPolicy.RUNTIME) @interface Container { R[] value(); }
  @R("a") @R("b") static void m() {}
  public static void main(String[] x) throws Exception {
    Method mm = RepeatProbe.class.getDeclaredMethod("m");
    R[] rs = mm.getAnnotationsByType(R.class);
    System.out.println("count="+rs.length);
    for (R r : rs) System.out.println("R.value()=" + r.value() + " class=" + r.getClass().getName());
    System.out.println("REPEAT_OK");
  }
}
