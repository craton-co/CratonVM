// Interpreter round i2 wave 56, lane L1: what the C JVMTI table's class
// functions answer for a primitive class (`int.class`, `void.class`) and,
// as a control, for an array class (`int[].class`), against HotSpot.
//
// The shim's `Agent_OnLoad` adds `can_get_source_file_name`; the native
// `facts` asks one class for its signature, status, modifiers, methods,
// fields, interfaces, kind (interface, array), loader, source file, class
// file version and whether it is modifiable. `main` asks `int.class`,
// `void.class`, `int[].class`, then `int.class` again (the first-call row).
//
// Needs the native shim tools/probes/interp/L1/L1W56JvmtiPrimitiveClass.c,
// loaded both as an agent and with `System.load` (for the native):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w56prim.so tools/probes/interp/L1/L1W56JvmtiPrimitiveClass.c
//   javac -d /tmp/l1w56prim tools/probes/interp/L1/L1W56JvmtiPrimitiveClass.java
//   java     -agentpath:/tmp/libl1w56prim.so -cp /tmp/l1w56prim L1W56JvmtiPrimitiveClass /tmp/libl1w56prim.so
//   cratonvm -agentpath:/tmp/libl1w56prim.so -cp /tmp/l1w56prim L1W56JvmtiPrimitiveClass /tmp/libl1w56prim.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W56JvmtiPrimitiveClass (README
// in tools/probes/interp/L1/rust-ports/).
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   int:
//   onload: add=0
//   signature: err=0 sig=I generic=null
//   status: err=0 value=32
//   modifiers: err=0 value=0x411
//   methods: err=0 count=0
//   fields: err=0 count=0
//   interfaces: err=0 count=0
//   interface: err=0 value=0
//   array: err=0 value=0
//   loader: err=0 null=1
//   source file: err=101
//   version: err=101
//   modifiable: err=0 value=0
//   void:
//   signature: err=0 sig=V generic=null
//   status: err=0 value=32
//   modifiers: err=0 value=0x411
//   methods: err=0 count=0
//   fields: err=0 count=0
//   interfaces: err=0 count=0
//   interface: err=0 value=0
//   array: err=0 value=0
//   loader: err=0 null=1
//   source file: err=101
//   version: err=101
//   modifiable: err=0 value=0
//   int[]:
//   signature: err=0 sig=[I generic=null
//   status: err=0 value=16
//   modifiers: err=0 value=0x431
//   methods: err=0 count=0
//   fields: err=0 count=0
//   interfaces: err=0 count=0
//   interface: err=0 value=0
//   array: err=0 value=1
//   loader: err=0 null=1
//   source file: err=101
//   version: err=101
//   modifiable: err=0 value=0
//   int again:
//   signature: err=0 sig=I generic=null
//   status: err=0 value=32
//   modifiers: err=0 value=0x411
//   methods: err=0 count=0
//   fields: err=0 count=0
//   interfaces: err=0 count=0
//   interface: err=0 value=0
//   array: err=0 value=0
//   loader: err=0 null=1
//   source file: err=101
//   version: err=101
//   modifiable: err=0 value=0
//
// CratonVM before wave 56 (code read): every row of `int` and `void` but
// `modifiable` answered `err=21` (a primitive class's mirror has no
// `ClassId`); the `int[]` rows are the control. Since wave 56
// (`CRATONVM_JVMTI_C_PRIMITIVE_CLASSES` on, the default from the lane's last
// commit): HotSpot's lines (`jvmti::native_env::or_primitive_class`).
public class L1W56JvmtiPrimitiveClass {
    static native String facts(Class<?> klass);

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W56JvmtiPrimitiveClass <path of the shim library>");
            return;
        }
        System.load(args[0]);
        System.out.println("int:");
        System.out.print(facts(int.class));
        System.out.println("void:");
        System.out.print(facts(void.class));
        System.out.println("int[]:");
        System.out.print(facts(int[].class));
        System.out.println("int again:");
        System.out.print(facts(int.class));
    }
}
