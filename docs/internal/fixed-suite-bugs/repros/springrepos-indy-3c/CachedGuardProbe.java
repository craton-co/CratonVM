import java.lang.invoke.*;

// Replicate Groovy IndyInterface.fallback()'s FULL cached-handle construction,
// which GuardProbe omits: the guarded handle is wrapped in
//   .asSpreader(Object[].class, arguments.length).asType((Object[])Object)
// and then invoked via invokeExact((Object[]) args) (as fromCache does).
// This is the path that triggers the sameClasses AIOOBE in the real run.
public class CachedGuardProbe {
  public static boolean sameClasses(Class<?>[] cs, Object[] os) {
    for (int i = 0; i < cs.length; i++) {
      Object o = os[i];
      if (o == null ? cs[i] != null : o.getClass() != cs[i]) return false;
    }
    return true;
  }
  // The "target" instance method: receiver + Closure-like arg -> Object
  public Object addRepositories(Object a) { return "called:" + a; }

  static Object fallbackTarget(Object recv, Object a) { return "fallback"; }

  public static void main(String[] x) throws Throwable {
    MethodHandles.Lookup l = MethodHandles.lookup();

    MethodHandle same = l.findStatic(CachedGuardProbe.class, "sameClasses",
        MethodType.methodType(boolean.class, Class[].class, Object[].class));

    // targetType for an instance call addRepositories(Object): (Recv, arg)Object -> pc=2
    MethodType targetType = MethodType.methodType(Object.class, CachedGuardProbe.class, Object.class);

    MethodHandle target = l.findVirtual(CachedGuardProbe.class, "addRepositories",
        MethodType.methodType(Object.class, Object.class));
    System.out.println("target.type=" + target.type() + " pc=" + target.type().parameterCount());
    target = target.asType(targetType);
    System.out.println("target(asType).type=" + target.type() + " pc=" + target.type().parameterCount());

    MethodHandle fallback = l.findStatic(CachedGuardProbe.class, "fallbackTarget",
        MethodType.methodType(Object.class, Object.class, Object.class)).asType(targetType);

    // ---- setGuards bulk SAME_CLASSES path ----
    Object[] args = new Object[]{ new CachedGuardProbe(), "closure" }; // length 2
    Class<?>[] classes = new Class<?>[args.length];
    for (int i = 0; i < args.length; i++) classes[i] = args[i] == null ? null : args[i].getClass();
    System.out.println("classes.length=" + classes.length);

    Class<?>[] paramArray = target.type().parameterArray();
    System.out.println("paramArray.length=" + paramArray.length);

    MethodHandle guard = same.bindTo(classes)
        .asCollector(Object[].class, paramArray.length)
        .asType(MethodType.methodType(boolean.class, paramArray));
    System.out.println("guard.type=" + guard.type() + " pc=" + guard.type().parameterCount());

    MethodHandle handle = MethodHandles.guardWithTest(guard, target, fallback);
    System.out.println("handle.type=" + handle.type() + " pc=" + handle.type().parameterCount());

    // ---- fallback() cached-handle wrap ----
    MethodHandle cached = handle
        .asSpreader(Object[].class, args.length)
        .asType(MethodType.methodType(Object.class, Object[].class));
    System.out.println("cached.type=" + cached.type() + " pc=" + cached.type().parameterCount());

    // ---- fromCache invoke ----
    Object r = (Object) cached.invokeExact((Object[]) args);
    System.out.println("RESULT=" + r);
    System.out.println("CACHEDGUARDPROBE_OK");
  }
}
