/*
 * Interpreter round i4, wave 84, lane L3: Executable.getParameters0 for a
 * MethodParameters attribute whose entries carry flags but no names.
 *
 * javac 21+ emits MethodParameters WITHOUT `-parameters` for a member with a
 * MANDATED or SYNTHETIC parameter: name_index 0 for every entry, the flags
 * kept (an inner class constructor's outer instance is `final mandated`, an
 * enum's valueOf(String) parameter `mandated`, an enum constructor's two
 * leading parameters `synthetic`). HotSpot's JVM_GetMethodParameters answers
 * a Parameter per entry with a NULL name and those flags, so the JDK reports
 * hasRealParameterData: isImplicit()/isSynthetic() are real, getName() is
 * still argN, isNamePresent() false, and getAllGenericParameterTypes lines
 * the generic Signature (which omits the mandated outer instance) up with the
 * real parameters. CratonVM's getParameters0 answered null for such an
 * attribute: every flag read 0, and the inner constructor's generic parameter
 * type fell back to the erased `interface java.util.List`.
 *
 * Compile WITHOUT -parameters (that is the point).
 *
 * Run (both modes must print the HotSpot lines):
 *   javac -d out L3W84ParameterFlags.java
 *   cratonvm --jdk-only   -cp out L3W84ParameterFlags
 *   cratonvm --compatible -cp out L3W84ParameterFlags
 *   java                  -cp out L3W84ParameterFlags   (HotSpot 25)
 *
 * Expected HotSpot 25 output (derived from javac 25's attribute and the JDK
 * Executable/Parameter bodies):
 *   inner p0: name=arg0 present=false implicit=true synthetic=false
 *   inner p1: name=arg1 present=false implicit=false synthetic=false
 *   inner generic p1: java.util.List<java.lang.String>
 *   valueOf p0: name=arg0 present=false implicit=true synthetic=false
 *   plain p0: name=arg0 present=false implicit=false synthetic=false
 *   plain generic p0: java.util.List<java.lang.String>
 */
import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.lang.reflect.Parameter;
import java.util.List;

public class L3W84ParameterFlags {
    class Inner {
        Inner(List<String> xs) {}
    }

    enum Color { RED, GREEN }

    static void plain(List<String> xs) {}

    static String describe(Parameter p) {
        return "name=" + p.getName() + " present=" + p.isNamePresent()
                + " implicit=" + p.isImplicit() + " synthetic=" + p.isSynthetic();
    }

    public static void main(String[] args) throws Exception {
        Constructor<?> inner = Inner.class.getDeclaredConstructor(L3W84ParameterFlags.class, List.class);
        Parameter[] ip = inner.getParameters();
        System.out.println("inner p0: " + describe(ip[0]));
        System.out.println("inner p1: " + describe(ip[1]));
        System.out.println("inner generic p1: " + ip[1].getParameterizedType().getTypeName());

        Method valueOf = Color.class.getMethod("valueOf", String.class);
        System.out.println("valueOf p0: " + describe(valueOf.getParameters()[0]));

        Method plain = L3W84ParameterFlags.class.getDeclaredMethod("plain", List.class);
        Parameter pp = plain.getParameters()[0];
        System.out.println("plain p0: " + describe(pp));
        System.out.println("plain generic p0: " + pp.getParameterizedType().getTypeName());
    }
}
