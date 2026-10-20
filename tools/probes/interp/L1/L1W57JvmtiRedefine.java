// Interpreter round i2 wave 57, lane L1: the C JVMTI table's
// `RedefineClasses` (87) and `can_redefine_classes`, against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-lacks-the-functions-and-events-agents-call-first-FIXED-20261007.md).
//
// The shim's `Agent_OnLoad` adds `can_redefine_classes`; the native
// `redefine` redefines one class with the bytes it is handed and prints the
// error. The probe reads its nested class `Target`'s class file, swaps the
// string constant "one" for "two" (same length), redefines `Target` with it
// and calls `Target.v()`, then redefines it back. The native `errors` prints
// the refusals: a negative count, a NULL definitions array, a NULL class, a
// NULL class file, a primitive and an array class, bytes that are no class
// file, another class's file, and an env obtained in the live phase without
// the capability (what it may hold first). Twice (the first-call rows
// again).
//
// Needs the native shim tools/probes/interp/L1/L1W57JvmtiRedefine.c, loaded
// both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w57redefine.so tools/probes/interp/L1/L1W57JvmtiRedefine.c
//   javac -d /tmp/l1w57redefine tools/probes/interp/L1/L1W57JvmtiRedefine.java
//   java     -agentpath:/tmp/libl1w57redefine.so -cp /tmp/l1w57redefine L1W57JvmtiRedefine /tmp/libl1w57redefine.so
//   cratonvm -agentpath:/tmp/libl1w57redefine.so -cp /tmp/l1w57redefine L1W57JvmtiRedefine /tmp/libl1w57redefine.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W57JvmtiRedefine (README in
// tools/probes/interp/L1/rust-ports/).
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   round 1
//   before: one
//   to two: onload=0 err=0
//   after: two
//   back: onload=0 err=0
//   again: one
//   errors: negative=103 null-defs=100 null-class=21 null-bytes=100 primitive=79 array=79 not-a-class=60 other-class=69 live-env=0 potential=1 err=99
//   still: one
//   round 2
//   (the eight rows again, the same)
//
// CratonVM before wave 57 (code read): `onload=98` (`can_redefine_classes`
// not potential), every `RedefineClasses` row `err=98` (slot 87
// unimplemented), `after: one`, and `potential=0 err=98`. Wave 57 (MEASURED,
// debug build): HotSpot's lines in both modes with
// `CRATONVM_JVMTI_C_REDEFINE` on (the default from the lane's last commit);
// with it off (`=0`), `onload=98`, every row `err=99`, `potential=0`.
import java.io.InputStream;

public class L1W57JvmtiRedefine {
    static native String redefine(String label, Class<?> k, byte[] bytes);
    static native String errors(Class<?> k, byte[] bytes, byte[] other);

    static class Target {
        static String v() {
            return "one";
        }
    }

    static byte[] classFile(Class<?> c) throws Exception {
        String name = c.getName();
        String resource = name.substring(name.lastIndexOf('.') + 1) + ".class";
        try (InputStream in = c.getResourceAsStream(resource)) {
            return in.readAllBytes();
        }
    }

    /** `bytes` with the first CONSTANT_Utf8 "from" swapped for "to" (same length). */
    static byte[] swap(byte[] bytes, String from, String to) {
        byte[] out = bytes.clone();
        byte[] f = from.getBytes();
        byte[] t = to.getBytes();
        for (int i = 0; i + 3 + f.length <= out.length; i++) {
            if (out[i] != 1 || out[i + 1] != 0 || out[i + 2] != f.length) {
                continue;
            }
            boolean match = true;
            for (int j = 0; j < f.length; j++) {
                if (out[i + 3 + j] != f[j]) {
                    match = false;
                    break;
                }
            }
            if (match) {
                System.arraycopy(t, 0, out, i + 3, t.length);
                return out;
            }
        }
        throw new IllegalStateException("no constant " + from);
    }

    static void round(long n) throws Exception {
        System.out.println("round " + n);
        byte[] original = classFile(Target.class);
        byte[] two = swap(original, "one", "two");
        System.out.println("before: " + Target.v());
        System.out.print(redefine("to two", Target.class, two));
        System.out.println("after: " + Target.v());
        System.out.print(redefine("back", Target.class, original));
        System.out.println("again: " + Target.v());
        System.out.print(errors(Target.class, original, classFile(L1W57JvmtiRedefine.class)));
        System.out.println("still: " + Target.v());
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W57JvmtiRedefine <path of the shim library>");
            return;
        }
        System.load(args[0]);
        round(1);
        round(2);
    }
}
