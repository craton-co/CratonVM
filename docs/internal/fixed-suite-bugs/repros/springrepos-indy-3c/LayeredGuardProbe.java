import java.lang.invoke.*;

// Reproduce the REAL Groovy setGuards compounding: a guardWithTest is applied
// FIRST (e.g. SAME_MC / switchpoint), THEN paramArray is read from the wrapped
// handle's type() to size the SAME_CLASSES collector, while classes[] is built
// from the runtime args. If guardWithTest shrinks the param count, the collector
// is too small and sameClasses(cs, os) indexes past os -> AIOOBE.
public class LayeredGuardProbe {
  public static boolean sameClasses(Class<?>[] cs, Object[] os) {
    for (int i = 0; i < cs.length; i++) {
      Object o = os[i];
      if (o == null ? cs[i] != null : o.getClass() != cs[i]) return false;
    }
    return true;
  }
  public static boolean alwaysTrue(Object a, Object b) { return true; }
  public Object addRepositories(Object a) { return "called:" + a; }
  static Object fallbackTarget(Object recv, Object a) { return "fallback"; }

  public static void main(String[] x) throws Throwable {
    MethodHandles.Lookup l = MethodHandles.lookup();
    MethodType targetType = MethodType.methodType(Object.class, LayeredGuardProbe.class, Object.class);

    MethodHandle target = l.findVirtual(LayeredGuardProbe.class, "addRepositories",
        MethodType.methodType(Object.class, Object.class)).asType(targetType);
    MethodHandle fallback = l.findStatic(LayeredGuardProbe.class, "fallbackTarget",
        MethodType.methodType(Object.class, Object.class, Object.class)).asType(targetType);

    // --- FIRST guard layer (like SAME_MC / switchpoint) ---
    MethodHandle test1 = l.findStatic(LayeredGuardProbe.class, "alwaysTrue",
        MethodType.methodType(boolean.class, Object.class, Object.class)).asType(
        MethodType.methodType(boolean.class, LayeredGuardProbe.class, Object.class));
    MethodHandle handle = MethodHandles.guardWithTest(test1, target, fallback);
    System.out.println("after layer1: handle.type pc=" + handle.type().parameterCount());

    // --- Now read paramArray from the WRAPPED handle (as setGuards does) ---
    Class<?>[] paramArray = handle.type().parameterArray();
    System.out.println("paramArray.length=" + paramArray.length);

    // --- SAME_CLASSES guard built from runtime args ---
    Object[] args = new Object[]{ new LayeredGuardProbe(), "closure" }; // length 2
    Class<?>[] classes = new Class<?>[args.length];
    for (int i = 0; i < args.length; i++) classes[i] = args[i].getClass();
    System.out.println("classes.length=" + classes.length);

    MethodHandle same = l.findStatic(LayeredGuardProbe.class, "sameClasses",
        MethodType.methodType(boolean.class, Class[].class, Object[].class));
    MethodHandle guard2 = same.bindTo(classes)
        .asCollector(Object[].class, paramArray.length)
        .asType(MethodType.methodType(boolean.class, paramArray));
    MethodHandle handle2 = MethodHandles.guardWithTest(guard2, handle, fallback);
    System.out.println("after layer2: handle2.type pc=" + handle2.type().parameterCount());

    MethodHandle cached = handle2
        .asSpreader(Object[].class, args.length)
        .asType(MethodType.methodType(Object.class, Object[].class));

    Object r = (Object) cached.invokeExact((Object[]) args);
    System.out.println("RESULT=" + r);
    System.out.println("LAYEREDGUARDPROBE_OK");
  }
}
