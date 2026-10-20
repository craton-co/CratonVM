// gci wave i15, lane rb: JNI `IsAssignableFrom` over primitive mirrors passed
// as `Class` parameters
// (docs/known-issues/gc/gci-i15rb-vm-default-path-changes-review-findings-20261014.md,
// finding 5).
//
// Since gci i14/jh, `jclass_class_id` answers `JNI_NO_CLASS` for an indirect
// local that resolves to no class id -- which a primitive mirror (`int.class`,
// `void.class`) is -- so `jni_is_assignable_from`'s `sub_id == sup_id` answered
// TRUE for ANY two primitive mirrors (`int` -> `long`). HotSpot answers a pair
// with a primitive mirror by mirror identity.
//
//   cl /LD /O1 /I"%JDK%\include" /I"%JDK%\include\win32" GciRb15PrimitiveAssignableProbe.c /Fegcirb15asg.dll
//   java     -XX:+UseSerialGC -cp <classes> GciRb15PrimitiveAssignableProbe <dll>
//   cratonvm -cp <classes> GciRb15PrimitiveAssignableProbe <dll>
//
// Expected stdout: HotSpot's (36 lines `<sub> -> <sup> <bool>`; deterministic).
public class GciRb15PrimitiveAssignableProbe {
    static native boolean assignable(Class<?> sub, Class<?> sup);

    public static void main(String[] args) {
        System.load(args[0]);
        Class<?>[] cs = {int.class, long.class, void.class, Object.class, Integer.class, int[].class};
        String[] names = {"int", "long", "void", "Object", "Integer", "int[]"};
        for (int i = 0; i < cs.length; i++) {
            for (int j = 0; j < cs.length; j++) {
                System.out.println(names[i] + " -> " + names[j] + " " + assignable(cs[i], cs[j]));
            }
        }
    }
}
