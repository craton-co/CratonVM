// Interpreter round i1 wave 41, lane L1: the generic signatures the JVMTI C
// table answers. `GetClassSignature` and `GetMethodName` must hand back a
// class's or method's `Signature` attribute (JVMS §4.7.9) through their
// `generic_ptr`, and NULL when there is none (JVMTI specification, both
// functions). CratonVM's C table (`vm/src/jvmti/native_env.rs`) answered NULL
// always, so an agent reading generic types (a profiler naming
// `List<String>`, a coverage or mocking agent) saw none.
//
// Needs the native shim tools/probes/interp/L1/L1W41JvmtiGenericSignatures.c,
// a plain JNI library that asks `GetEnv` for a JVMTI env (no agent flag, no
// capability: both functions need none). Build it against the JDK's headers
// and pass its absolute path as the only argument:
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w41jvmtigen.so tools/probes/interp/L1/L1W41JvmtiGenericSignatures.c
//   javac -d /tmp/l1w41 tools/probes/interp/L1/L1W41JvmtiGenericSignatures.java
//   java     -cp /tmp/l1w41 L1W41JvmtiGenericSignatures /tmp/libl1w41jvmtigen.so
//   cratonvm -cp /tmp/l1w41 L1W41JvmtiGenericSignatures /tmp/libl1w41jvmtigen.so
//
// Without the argument it prints only the usage line (HotSpot and CratonVM
// alike, so the plain probe runner compares that).
//
// Expected stdout (HotSpot 25): the `Signature` attributes as `javap -v`
// prints them for these classes (JDK 25.0.3's javac and `java.base`). NOT RUN
// on HotSpot with the shim: the Windows box that wrote the probe has no C
// compiler; the orchestrator should run the HotSpot line above once and
// replace this note if anything differs.
//
//   class L1W41JvmtiGenericSignatures: sig=LL1W41JvmtiGenericSignatures; generic=<T:Ljava/lang/Number;>Ljava/lang/Object;
//   class L1W41JvmtiGenericSignatures$Plain: sig=LL1W41JvmtiGenericSignatures$Plain; generic=NULL
//   class java.util.ArrayList: sig=Ljava/util/ArrayList; generic=<E:Ljava/lang/Object;>Ljava/util/AbstractList<TE;>;Ljava/util/List<TE;>;Ljava/util/RandomAccess;Ljava/lang/Cloneable;Ljava/io/Serializable;
//   class [Ljava.lang.String;: sig=[Ljava/lang/String; generic=NULL
//   method pick: name=pick sig=(Ljava/lang/Object;)Ljava/lang/Object; generic=<U:Ljava/lang/Object;>(TU;)TU;
//   method names: name=names sig=()Ljava/util/List; generic=()Ljava/util/List<Ljava/lang/String;>;
//   method plain: name=plain sig=(I)I generic=NULL
//   method ArrayList.get: name=get sig=(I)Ljava/lang/Object; generic=(I)TE;
//
// CratonVM before wave 41: every `generic=` above was `generic=NULL`.
// `--compatible` may differ on the `java.util.ArrayList` rows if that mode
// serves the class from its own stand-in without the attribute (not traced).
import java.util.ArrayList;
import java.util.List;

public class L1W41JvmtiGenericSignatures<T extends Number> {
    static final class Plain {
    }

    <U> U pick(U u) {
        return u;
    }

    static List<String> names() {
        return new ArrayList<>();
    }

    static int plain(int x) {
        return x;
    }

    /** `GetClassSignature(c)` as `sig=... generic=...`, or `error=<n>`. */
    static native String classSignature(Class<?> c);

    /**
     * `GetMethodName` of the method `c` declares under `name` and `desc`
     * (`GetStaticMethodID` when `isStatic`, else `GetMethodID`), as
     * `name=... sig=... generic=...`, or `error=<n>` / `no method`.
     */
    static native String methodSignature(Class<?> c, String name, String desc, boolean isStatic);

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W41JvmtiGenericSignatures <absolute path of the native shim>");
            return;
        }
        System.load(args[0]);
        Class<?>[] classes = {
            L1W41JvmtiGenericSignatures.class, Plain.class, ArrayList.class, String[].class,
        };
        for (Class<?> c : classes) {
            System.out.println("class " + c.getName() + ": " + classSignature(c));
        }
        Class<?> me = L1W41JvmtiGenericSignatures.class;
        System.out.println("method pick: "
                + methodSignature(me, "pick", "(Ljava/lang/Object;)Ljava/lang/Object;", false));
        System.out.println("method names: " + methodSignature(me, "names", "()Ljava/util/List;", true));
        System.out.println("method plain: " + methodSignature(me, "plain", "(I)I", true));
        System.out.println("method ArrayList.get: "
                + methodSignature(ArrayList.class, "get", "(I)Ljava/lang/Object;", false));
    }
}
