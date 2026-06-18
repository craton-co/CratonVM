import java.lang.reflect.*;
import java.util.*;

// bug-06 family 5 bisection: find the reflection method that returns null
// under CratonVM where HotSpot returns a value. Run under both VMs and diff.
// "Cannot invoke getDeclaredMethod on null" => some Class-returning reflection
// call yielded null. Probe every candidate and print a stable signature.
public class Refl5 {
  static class Inner { void f(){} }
  class NonStaticInner {}
  enum E { A, B { void g(){} }; void g(){} }
  static void p(String k, Object v){ System.out.println(k+" = "+v); }
  static String cn(Class<?> c){ return c==null?"<null>":c.getName(); }

  public static void main(String[] a) throws Throwable {
    // getSuperclass
    p("Object.superclass", cn(Object.class.getSuperclass()));            // null
    p("String.superclass", cn(String.class.getSuperclass()));            // Object
    p("ArrayList.superclass", cn(ArrayList.class.getSuperclass()));      // AbstractList
    p("int.superclass", cn(int.class.getSuperclass()));                  // null
    p("Comparable.superclass", cn(Comparable.class.getSuperclass()));    // null (iface)
    p("int[].superclass", cn(int[].class.getSuperclass()));              // Object
    p("E.superclass", cn(E.class.getSuperclass()));                      // Enum
    p("E.A.class.superclass", cn(E.A.getClass().getSuperclass()));       // E
    p("E.B.class.superclass", cn(E.B.getClass().getSuperclass()));       // E (const-body subclass)

    // getComponentType
    p("int[].component", cn(int[].class.getComponentType()));            // int
    p("String[].component", cn(String[].class.getComponentType()));      // String
    p("String[][].component", cn(String[][].class.getComponentType()));  // String[]
    p("String.component", cn(String.class.getComponentType()));          // null

    // getDeclaringClass / getEnclosingClass
    p("Inner.declaring", cn(Inner.class.getDeclaringClass()));           // Refl5
    p("Inner.enclosing", cn(Inner.class.getEnclosingClass()));           // Refl5
    p("Refl5.declaring", cn(Refl5.class.getDeclaringClass()));           // null
    p("Refl5.enclosing", cn(Refl5.class.getEnclosingClass()));           // null
    p("NonStaticInner.declaring", cn(NonStaticInner.class.getDeclaringClass())); // Refl5
    p("E.A.class.enclosing", cn(E.A.getClass().getEnclosingClass()));    // E or null (impl)
    p("E.declaring", cn(E.class.getDeclaringClass()));                   // Refl5

    // Method.getDeclaringClass
    Method m = ArrayList.class.getMethod("add", Object.class);
    p("ArrayList.add.declaring", cn(m.getDeclaringClass()));             // AbstractList/List
    Method tos = String.class.getMethod("toString");
    p("String.toString.declaring", cn(tos.getDeclaringClass()));        // String
    Method hc = Object.class.getMethod("hashCode");
    p("Object.hashCode.declaring", cn(hc.getDeclaringClass()));         // Object

    // Field / Constructor declaring class
    Constructor<?> c0 = ArrayList.class.getConstructor();
    p("ArrayList.<init>.declaring", cn(c0.getDeclaringClass()));        // ArrayList

    // getEnclosingMethod/Constructor return-class shape (lambdas, local classes)
    Runnable r = () -> {};
    p("lambda.class.enclosing", cn(r.getClass().getEnclosingClass()));   // Refl5 or null
    p("lambda.class.declaring", cn(r.getClass().getDeclaringClass()));   // null

    // getDeclaredMethod chained off the above (the actual failing op shape)
    p("Inner.enclosing.getDeclaredMethod(main)",
      cn(Inner.class.getEnclosingClass().getDeclaredMethod("main", String[].class).getDeclaringClass()));

    // Class.forName variants
    p("forName(String)", cn(Class.forName("java.lang.String")));
    p("forName(j.u.List)", cn(Class.forName("java.util.List")));

    System.out.println("DONE");
  }
}
