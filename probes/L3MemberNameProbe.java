import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandleInfo;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

/**
 * L3 (jdk-only wave 2) — behavioural oracle for the `java.lang.invoke.MemberName`
 * row of the fabricated-layout census.
 *
 * `MemberName` is package-private, so nothing here can name it. That is the
 * point: everything CratonVM's `MethodHandleNatives` natives put into a
 * MemberName has to come back out through this public surface, and the host JDK
 * is the oracle for all of it.
 *
 * The census row was slot 4, `Int` over `L`, 7 hits per `JdkOnlyBreadthProbe`
 * run. Slot 4 on a real `MemberName` is `method`, a `ResolvedMethodName`
 * reference; the natives were writing a `vmindex` sentinel there, and `vmindex`
 * is `@Injected` in HotSpot — the class file declares no field for it at all.
 *
 * The four writers this exercises: `MethodHandleNatives.init` (via `unreflect`
 * of a Method/Field/Constructor), `MethodHandleNatives.resolve`,
 * `Lookup.revealDirect`, and `alloc_resolved_member_name` (reached from
 * LambdaForm preparation on any method-handle invocation).
 */
public final class L3MemberNameProbe {

    public static class Bean {
        public int value = 7;
        public int getValue() { return value; }
        public static String stat(int n) { return "s" + n; }
        public Bean() {}
        public Bean(int v) { this.value = v; }
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();

        // findVirtual / findStatic, then invoke — drives LambdaForm preparation
        // and so `alloc_resolved_member_name`.
        MethodHandle getValue = l.findVirtual(Bean.class, "getValue", MethodType.methodType(int.class));
        System.out.println("mh.virtual.type=" + getValue.type());
        System.out.println("mh.virtual.invoke=" + (int) getValue.invoke(new Bean()));

        MethodHandle stat = l.findStatic(Bean.class, "stat",
                MethodType.methodType(String.class, int.class));
        System.out.println("mh.static.type=" + stat.type());
        System.out.println("mh.static.invoke=" + (String) stat.invoke(3));

        // revealDirect — builds a MemberName and wraps it in InfoFromMemberName.
        MethodHandleInfo info = l.revealDirect(getValue);
        System.out.println("reveal.name=" + info.getName());
        System.out.println("reveal.declaring=" + info.getDeclaringClass().getName());
        System.out.println("reveal.type=" + info.getMethodType());
        System.out.println("reveal.refKind=" + info.getReferenceKind());
        System.out.println("reveal.refKindName="
                + MethodHandleInfo.referenceKindToString(info.getReferenceKind()));

        MethodHandleInfo sinfo = l.revealDirect(stat);
        System.out.println("reveal.static.name=" + sinfo.getName());
        System.out.println("reveal.static.refKind=" + sinfo.getReferenceKind());

        // unreflect* — drives MethodHandleNatives.init from a reflected member.
        Method m = Bean.class.getMethod("getValue");
        MethodHandle um = l.unreflect(m);
        System.out.println("unreflect.method.type=" + um.type());
        System.out.println("unreflect.method.invoke=" + (int) um.invoke(new Bean()));

        Field f = Bean.class.getField("value");
        MethodHandle getter = l.unreflectGetter(f);
        System.out.println("unreflect.getter.type=" + getter.type());
        System.out.println("unreflect.getter.invoke=" + (int) getter.invoke(new Bean()));

        Constructor<Bean> c = Bean.class.getConstructor(int.class);
        MethodHandle ctor = l.unreflectConstructor(c);
        System.out.println("unreflect.ctor.type=" + ctor.type());
        System.out.println("unreflect.ctor.invoke=" + ((Bean) ctor.invoke(11)).value);

        // A field VarHandle-free field accessor pair, then a setter round trip.
        MethodHandle setter = l.unreflectSetter(f);
        Bean b = new Bean();
        setter.invoke(b, 42);
        System.out.println("unreflect.setter.roundtrip=" + b.value);

        System.out.println("L3MemberNameProbe done");
    }
}
