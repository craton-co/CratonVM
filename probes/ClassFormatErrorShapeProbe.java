/**
 * Is java.lang.ClassFormatError constructible and catchable at all on this VM?
 * Distinguishes "the interpreter never converted the VM-side LinkageError" from
 * "create_exception_object(java/lang/ClassFormatError) itself fails".
 */
public class ClassFormatErrorShapeProbe {

    public static void main(String[] args) throws Exception {
        Class<?> c = Class.forName("java.lang.ClassFormatError");
        System.out.println("forName            = " + c);
        System.out.println("superclass         = " + c.getSuperclass());
        try {
            throw new ClassFormatError("hand-thrown");
        }
        catch (ClassFormatError ex) {
            System.out.println("caught hand-thrown = " + ex);
        }
        Object viaReflection = c.getConstructor(String.class).newInstance("reflective");
        System.out.println("reflective ctor    = " + viaReflection);
        try {
            throw (Error) viaReflection;
        }
        catch (LinkageError ex) {
            System.out.println("caught as Linkage  = " + ex);
        }
        System.out.println("SHAPE PROBE PASS");
    }
}
