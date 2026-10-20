import java.lang.reflect.*;
import jdk.jfr.*;
public class Sbr5AwtjfrJfrUpcall {
  @Name("probe.Ev2") @Label("Probe2") static class Ev2 extends Event { @Label("n") int n; String s; }
  public static void main(String[] a) throws Throwable {
    byte[] old;
    try (var in = Sbr5AwtjfrJfrUpcall.class.getResourceAsStream("Sbr5AwtjfrJfrUpcall$Ev2.class")) { old = in.readAllBytes(); }
    System.out.println("old=" + old.length);
    Class<?> up = Class.forName("jdk.jfr.internal.JVMUpcalls");
    Method m = up.getDeclaredMethod("bytesForEagerInstrumentation", long.class, boolean.class, boolean.class, Class.class, byte[].class);
    m.setAccessible(true);
    byte[] nb = (byte[]) m.invoke(null, 4242L, true, false, Event.class, old);
    System.out.println("new=" + nb.length + " changed=" + !java.util.Arrays.equals(old, nb));
  }
}
