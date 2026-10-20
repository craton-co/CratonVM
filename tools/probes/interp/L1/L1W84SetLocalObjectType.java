// Interpreter round i4 wave 84, lane L1: JVMTI SetLocalObject refuses an
// object whose class does not fit the variable's LocalVariableTable
// signature with JVMTI_ERROR_TYPE_MISMATCH (34) and leaves the local alone,
// as HotSpot 25 does (jvmtiImpl.cpp VM_BaseGetOrSetLocal::check_slot_type_lvt
// -> is_assignable). CratonVM stored any object, so `target` went on with a
// String in an Integer variable. The same check now guards JDWP
// StackFrame.SetValues (raw JDWP; JDI checks before sending).
//
// Needs the native shim tools/probes/interp/L1/L1W84SetLocalObjectType.c,
// loaded both as an agent (for the OnLoad capability) and with System.load:
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w84setlocal.so tools/probes/interp/L1/L1W84SetLocalObjectType.c
//   javac -g -d /tmp/l1w84setlocal tools/probes/interp/L1/L1W84SetLocalObjectType.java
//   java     -agentpath:/tmp/libl1w84setlocal.so -cp /tmp/l1w84setlocal L1W84SetLocalObjectType /tmp/libl1w84setlocal.so
//   cratonvm -agentpath:/tmp/libl1w84setlocal.so -cp /tmp/l1w84setlocal L1W84SetLocalObjectType /tmp/libl1w84setlocal.so
//   cratonvm --compatible -agentpath:/tmp/libl1w84setlocal.so -cp /tmp/l1w84setlocal L1W84SetLocalObjectType /tmp/libl1w84setlocal.so
//
// (`-g` is load-bearing: without a LocalVariableTable HotSpot checks only the
// slot's kind. A `cratonvm` built with `--features experimental-debug`.)
//
// HOTSPOT_EXPECTED_BEGIN (derived from the JDK 25 sources, not measured)
// string->Integer=34 integer->Integer=0 string->Object=0 string->Runnable=34 integer->Number=0 string->int[]=34
// boxed=8 any=str task=null num=9 arr=1
// HOTSPOT_EXPECTED_END
//
// Before the fix CratonVM printed 0 for all six and then failed in `target`
// (a String read as Integer / Runnable / int[]).
public class L1W84SetLocalObjectType {
    static native String poke(String text, Integer eight, Integer nine);

    static void target() {
        Integer boxed = Integer.valueOf(7);
        Object any = Integer.valueOf(1);
        Runnable task = null;
        Number num = Long.valueOf(2);
        int[] arr = new int[1];
        System.out.println(poke("str", Integer.valueOf(8), Integer.valueOf(9)));
        System.out.println("boxed=" + boxed.intValue() + " any=" + any + " task=" + task
                + " num=" + num + " arr=" + arr.length);
    }

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W84SetLocalObjectType <path of the shim library>");
            return;
        }
        System.load(args[0]);
        target();
    }
}
