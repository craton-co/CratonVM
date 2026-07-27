import org.springframework.core.env.*;
import org.springframework.web.context.support.StandardServletEnvironment;
public class EnvProbe {
  public static void main(String[] a) {
    ConfigurableEnvironment env = new StandardServletEnvironment();
    MutablePropertySources s = env.getPropertySources();
    int i = 0;
    for (PropertySource<?> ps : s) System.out.println("  [" + (i++) + "] " + ps.getName() + "  (" + ps.getClass().getSimpleName() + ")");
    System.out.println("size=" + s.size());
    System.out.println("precedenceOf(systemProperties)=" + s.precedenceOf(PropertySource.named("systemProperties")));
  }
}
