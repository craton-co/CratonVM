// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 104, lane L4: `MethodHandleNatives.resolve` of an
// instance method a CLASS inherits from a superinterface takes the
// maximally-specific default, as HotSpot's resolution does (the class's
// `default_methods`; JVMS 5.4.3.3 step 3a), not the first declaration its
// interface walk meets.
//
// `abstract class Base implements Shape {}` and `class Leaf extends Base
// implements Rounded {}` where `Rounded extends Shape` re-declares `area()` as a
// default: CratonVM's walk popped `Shape.area` (abstract) first and wrote its
// ACC_ABSTRACT into the `MemberName`, so `revealDirect(..).getModifiers()` was
// 1025 (`public abstract`) where HotSpot answers 1 (`public`).
//
// Rows: the inherited-through-subclass case, the direct case (control), the
// declaring class `revealDirect` names, and the call's answer. `two-abstract`:
// `interface Both extends Left, Right`, each declaring abstract `int m()`;
// HotSpot resolves `findVirtual(Both.class, "m")` to the FIRST of its
// transitive-interface order (`Left`); CratonVM's LIFO walk named `Right`.
//
// Run: javac -d . L4W104DefaultThroughSubclassModifiers.java
//      java -cp . L4W104DefaultThroughSubclassModifiers                (HotSpot)
//      cratonvm --jdk-only -cp . L4W104DefaultThroughSubclassModifiers
//      cratonvm --compatible -cp . L4W104DefaultThroughSubclassModifiers
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the sources)
// leaf modifiers 1 public
// leaf declaring L4W104DefaultThroughSubclassModifiers$Leaf
// leaf call 2
// direct modifiers 1 public
// direct call 2
// abstract-only modifiers 1025 public abstract
// two-abstract declaring L4W104DefaultThroughSubclassModifiers$Left
// two-abstract kind invokeInterface
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandleInfo;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Modifier;

public class L4W104DefaultThroughSubclassModifiers {
    public interface Shape {
        int area();
    }

    public interface Rounded extends Shape {
        default int area() {
            return 2;
        }
    }

    public abstract static class Base implements Shape {
    }

    public static class Leaf extends Base implements Rounded {
    }

    public static class Direct implements Rounded {
    }

    public abstract static class OnlyAbstract implements Shape {
    }

    public interface Left {
        int m();
    }

    public interface Right {
        int m();
    }

    public interface Both extends Left, Right {
    }

    static void row(String name, Class<?> c, Object receiver) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandle mh = lookup.findVirtual(c, "area", MethodType.methodType(int.class));
        MethodHandleInfo info = lookup.revealDirect(mh);
        int mods = info.getModifiers();
        System.out.println(name + " modifiers " + mods + " " + Modifier.toString(mods));
        if (name.equals("leaf")) {
            System.out.println(name + " declaring " + info.getDeclaringClass().getName());
        }
        if (receiver != null) {
            System.out.println(name + " call " + (int) mh.invoke(receiver));
        }
    }

    public static void main(String[] args) throws Throwable {
        row("leaf", Leaf.class, new Leaf());
        row("direct", Direct.class, new Direct());
        row("abstract-only", OnlyAbstract.class, null);
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandleInfo both = lookup.revealDirect(
                lookup.findVirtual(Both.class, "m", MethodType.methodType(int.class)));
        System.out.println("two-abstract declaring " + both.getDeclaringClass().getName());
        System.out.println("two-abstract kind " + MethodHandleInfo.referenceKindToString(both.getReferenceKind()));
    }
}
