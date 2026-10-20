/*
 * Interpreter round i4, wave 84, lane L3: Class.forPrimitiveName (JDK 22+).
 *
 * JDK 25's body is a string `switch`: a null name is a NullPointerException
 * (javac's desugared `hashCode()` call; the message, derived from the
 * bytecode shape and not printed here, is
 * `Cannot invoke "String.hashCode()" because "<local1>" is null`), and every
 * name that is not one of the nine primitive keywords answers null. The
 * native CratonVM registers for this triple (`native_class_get_primitive_class`,
 * reached wherever the registration is preferred to the body) answered null
 * for a null name.
 *
 * Run (both modes must print the HotSpot lines):
 *   javac -d out L3W84ClassNullNames.java
 *   cratonvm --jdk-only   -cp out L3W84ClassNullNames
 *   cratonvm --compatible -cp out L3W84ClassNullNames
 *   java                  -cp out L3W84ClassNullNames   (HotSpot 25)
 *
 * Expected HotSpot 25 output:
 *   int: int true
 *   void: void true
 *   Integer: null
 *   empty: null
 *   null: java.lang.NullPointerException
 */
public class L3W84ClassNullNames {
    static String ask(String name) {
        try {
            Class<?> c = Class.forPrimitiveName(name);
            return c == null ? "null" : c.getName() + " " + c.isPrimitive();
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    public static void main(String[] args) {
        System.out.println("int: " + ask("int"));
        System.out.println("void: " + ask("void"));
        System.out.println("Integer: " + ask("java.lang.Integer"));
        System.out.println("empty: " + ask(""));
        System.out.println("null: " + ask(null));
    }
}
