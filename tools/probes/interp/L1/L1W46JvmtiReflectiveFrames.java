// Interpreter round i1 wave 46, lane L1: a reflective call's JDK frames in
// the C JVMTI table's stack functions, against HotSpot (item 4 of
// docs/known-issues/interpreter/i43-L3-a-throwable-a-reflective-native-raises-lists-no-reflection-frames-20261007.md).
//
// `main` calls `viaMethod` and `viaConstructor`. `viaMethod` calls
// `target` through `Method.invoke`, `viaConstructor` constructs `Made`
// through `Constructor.newInstance`; each calls the shim's native `rows`,
// which lists its own thread's stack with `GetStackTrace` (from depth 0, at
// most 12 frames), one frame per line as `<class>.<method>@<location>`
// (location -1 for a native method). JDK frames are printed as they are,
// except the frames HotSpot runs between the accessor and the target that a
// stack trace hides: the accessor's `@Hidden` `invokeImpl` and the
// method-handle frames under it (`DirectMethodHandle$Holder`,
// `LambdaForm$...`), whose names and count are HotSpot's own and which this
// VM does not run; they are left out.
//
// Needs the native shim tools/probes/interp/L1/L1W46JvmtiReflectiveFrames.c,
// loaded both as an agent and with `System.load` (for `rows`):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w46refl.so tools/probes/interp/L1/L1W46JvmtiReflectiveFrames.c
//   javac -d /tmp/l1w46refl tools/probes/interp/L1/L1W46JvmtiReflectiveFrames.java
//   java     -agentpath:/tmp/libl1w46refl.so -cp /tmp/l1w46refl L1W46JvmtiReflectiveFrames /tmp/libl1w46refl.so
//   cratonvm -agentpath:/tmp/libl1w46refl.so -cp /tmp/l1w46refl L1W46JvmtiReflectiveFrames /tmp/libl1w46refl.so
//
// Without the argument it prints only the usage line.
//
// Expected stdout (HotSpot 25.0.3): EXPECTED_BLOCK
public class L1W46JvmtiReflectiveFrames {
    static native String rows(String label);

    public static String target() {
        return rows("method");
    }

    public static final class Made {
        public String listed;

        public Made() {
            listed = rows("constructor");
        }
    }

    static String viaMethod() throws Exception {
        return (String) L1W46JvmtiReflectiveFrames.class.getMethod("target").invoke(null);
    }

    static String viaConstructor() throws Exception {
        return Made.class.getConstructor().newInstance().listed;
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W46JvmtiReflectiveFrames <path of the shim library>");
            return;
        }
        System.load(args[0]);
        System.out.print(viaMethod());
        System.out.print(viaConstructor());
    }
}
