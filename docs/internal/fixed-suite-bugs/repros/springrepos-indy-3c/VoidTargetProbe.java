import java.lang.invoke.*;

// Groovy indy call sites return Object even when the resolved method is void.
// Reproduce: a VOID target method invoked through an Object-returning cached MH
// (asType to (Object[])Object) via invokeExact -> the void->null adaptation must
// yield null, not "no value" (which underflows the caller's areturn).
public class VoidTargetProbe {
  public void addRepositories(Object a) { /* void */ }

  public static void main(String[] x) throws Throwable {
    MethodHandles.Lookup l = MethodHandles.lookup();
    // call site type returns Object (Groovy), target method is void
    MethodType callType = MethodType.methodType(Object.class, VoidTargetProbe.class, Object.class);
    MethodHandle target = l.findVirtual(VoidTargetProbe.class, "addRepositories",
        MethodType.methodType(void.class, Object.class));
    System.out.println("target.type=" + target.type());
    target = target.asType(callType); // (Recv,Object)Object  -- void adapted to null
    System.out.println("target(asType).type=" + target.type());

    // Wrap in guardWithTest so the handle's effective return type is Object (L),
    // exactly like Groovy's selector.handle -- this is what makes auto_box_return
    // see an 'L' return while the dispatched target is void.
    MethodHandle test = MethodHandles.dropArguments(
        MethodHandles.constant(boolean.class, true), 0, VoidTargetProbe.class, Object.class);
    MethodHandle fallback = MethodHandles.dropArguments(
        MethodHandles.constant(Object.class, "fb"), 0, VoidTargetProbe.class, Object.class);
    MethodHandle handle = MethodHandles.guardWithTest(test, target, fallback);
    System.out.println("handle.type=" + handle.type());

    Object[] args = new Object[]{ new VoidTargetProbe(), "closure" };
    MethodHandle cached = handle
        .asSpreader(Object[].class, args.length)
        .asType(MethodType.methodType(Object.class, Object[].class));
    Object r = (Object) cached.invokeExact((Object[]) args);
    System.out.println("RESULT=" + r);
    System.out.println("VOIDTARGETPROBE_OK");
  }
}
