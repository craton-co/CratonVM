// Interpreter round i5 wave 103, lane L1: the C JVMTI table's `GetLocal*` /
// `SetLocal*` at a bytecode index equal to a variable's `start_pc + length`
// (the first instruction after its block). Proves (or refutes)
// docs/internal/fixed-bugs/interpreter-i100-L1-jvmti-local-scope-end-is-inclusive-FIXED-20261016.md.
//
// HotSpot (`jvmtiImpl.cpp`, `VM_BaseGetOrSetLocal::check_slot_type_lvt`)
// takes the first table entry with `start_bci <= bci <= start_bci + length`
// (inclusive end): the slot is readable there, typed by that entry. Wave 100
// moved CratonVM's `native_env::declared_local` /
// `declared_reference_signature` to the inclusive end (it answered
// INVALID_SLOT, 35); read from the source, never measured.
//
// `m` is, with `javac -g`:
//
//   0 bipush 7; 2 istore_0; 3 iload_0; 4 putstatic sink; 7 invokestatic rows; ...
//
// so `x` (slot 0) has start_pc 3, length 4, and `m`'s frame (depth 1 inside
// the native `rows`) stands at bci 7 = its scope's end. The shim checks that
// itself (`at the end of x's scope: 1`) from GetFrameLocation and
// GetLocalVariableTable.
//
// Needs the native shim tools/probes/interp/L1/L1W103JvmtiLocalScopeEnd.c,
// loaded both as an agent and with `System.load`. Build and run (Linux, gcc;
// or tools/jvmti/run-jvmti-shims.sh --scenario L1W103JvmtiLocalScopeEnd):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w103scope.so tools/probes/interp/L1/L1W103JvmtiLocalScopeEnd.c
//   javac -g -d /tmp/l1w103scope tools/probes/interp/L1/L1W103JvmtiLocalScopeEnd.java
//   java     -agentpath:/tmp/libl1w103scope.so -cp /tmp/l1w103scope L1W103JvmtiLocalScopeEnd /tmp/libl1w103scope.so
//   cratonvm -agentpath:/tmp/libl1w103scope.so -cp /tmp/l1w103scope L1W103JvmtiLocalScopeEnd /tmp/libl1w103scope.so
//   cratonvm --compatible -agentpath:/tmp/libl1w103scope.so -cp /tmp/l1w103scope L1W103JvmtiLocalScopeEnd /tmp/libl1w103scope.so
//
// (`-g` matters.) Without the argument it prints only the usage line.
//
// Expected stdout (HotSpot 25; derived from jvmtiImpl.cpp, NOT measured --
// run the HotSpot line once and correct this block if anything differs):
//
//   frame: m at the end of x's scope: 1
//   GetLocalInt x: err=0 value=7
//   GetLocalObject x: err=34
//   GetLocalFloat x: err=34
//   SetLocalInt x=9: err=0
//   GetLocalInt x after the write: err=0 value=9
//   done
//
// If HotSpot answers err=35 on the four GetLocal/SetLocal rows, its end is
// exclusive: revert wave 100 as the page says.
//
// CratonVM before wave 100 (from the code): `err=35` on the five
// GetLocal*/SetLocal* rows.
public class L1W103JvmtiLocalScopeEnd {
    static native String rows();

    static int sink;
    static String report;

    static int m() {
        {
            int x = 7;
            sink = x;
        }
        report = rows();
        return 0;
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W103JvmtiLocalScopeEnd <path of the shim>");
            return;
        }
        System.load(args[0]);
        m();
        System.out.print(report);
        System.out.println("done");
    }
}
