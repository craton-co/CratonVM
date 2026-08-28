// The other half of JVMS §5.4.3.2: when the recorded (name, descriptor) pair
// exists NOWHERE in the hierarchy, resolution fails with NoSuchFieldError.
//
// Compiled against a `C` declaring `Object x`, so the fieldref records
// `C.x:Ljava/lang/Object;`. Then run against a `C` whose `x` is an `int` and
// which has no `Object x` anywhere. The pair is absent, and the spec answer is
// NoSuchFieldError -- not "here is the int field of the same name", which is
// what this VM returned before 2026-08-28 and what
// CRATONVM_FIELD_RESOLUTION_NAME_ONLY=1 restores.
public class StrictMain {
    public static void main(String[] args) {
        try {
            C c = new C();
            Object o = c.x;
            System.out.println("PROBE-FAIL resolved to a same-named field of another type: " + o);
        } catch (NoSuchFieldError e) {
            System.out.println("PROBE-PASS NoSuchFieldError: " + e.getMessage());
        }
    }
}
