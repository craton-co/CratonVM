// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L8 -- the serialization constructor
// (`sun.reflect.ReflectionFactory.newConstructorForSerialization`, the shape
// Objenesis / Mockito / Kryo use) as the VM serves it. JDK 25 builds it with
// `MethodHandleAccessorFactory.newSerializableConstructorAccessor`
// (`constructorInSuperclass` check, `ensureClassInitialized(decl)`, then a
// `DirectMethodHandle.makeAllocator(decl, ctor)` handle behind a
// `DirectConstructorHandleAccessor`); CratonVM serves `Constructor.newInstance`
// of it natively (`lang_class.rs`, the serialization arm).
//
//   plain / plain-ctor   Object's constructor over a non-serializable class:
//                        an instance of the class, its own constructor and
//                        field initializers do not run; the Constructor is a
//                        copy of Object's (accessible)
//   ancestor             an intermediate ancestor's no-arg constructor runs,
//                        the leaf's does not
//   same-class           the class's own constructor comes back as is
//   unrelated            a constructor of a non-superclass: the eager
//                        UnsupportedOperationException
//   abstract             an abstract class: the handle's
//                        Unsafe.allocateInstance refuses, wrapped as
//                        InvocationTargetException (CratonVM allocated an
//                        instance of the abstract class -- FIXED wave 98)
//   init-at-create/new   the class is initialized when the constructor is
//                        MADE, not when it is invoked
//   eiie / eiie-again    a failing <clinit> propagates unwrapped from
//                        newConstructorForSerialization, then NoClassDefFoundError
//   argcount             newInstance(1) on the nullary serialization ctor:
//                        IllegalArgumentException (CratonVM ignored the
//                        argument -- FIXED wave 98)
//   one-arg-*            newConstructorForSerialization(cl): null when the
//                        first non-serializable superclass's no-arg ctor is
//                        private, a constructor when it is package-private in
//                        the same package
//
// Run (stdout must be the block below, byte for byte; sun.reflect is exported
// by jdk.unsupported, no flags needed):
//   $J/javac -d out tools/probes/interp/L8/L8W98SerialCtorShapes.java
//   $B --java-home $J/.. -cp out L8W98SerialCtorShapes
//   $B --compatible --java-home $J/.. -cp out L8W98SerialCtorShapes
//   (each also with --nojit)
//   $J/java -cp out L8W98SerialCtorShapes
//
// HOTSPOT_EXPECTED_BEGIN (derived from the JDK 25 sources, not measured)
// plain L8W98SerialCtorShapes$Plain v=0 log=
// plain-ctor java.lang.Object params=0 accessible=true
// ancestor L8W98SerialCtorShapes$Leaf w=0 log=Base2()
// same-class true L8W98SerialCtorShapes$Plain v=5 log=Plain()
// unrelated java.lang.UnsupportedOperationException: public java.lang.String() not a superclass of L8W98SerialCtorShapes$Plain
// abstract java.lang.reflect.InvocationTargetException cause=java.lang.InstantiationException: L8W98SerialCtorShapes$Abs
// init-at-create Lazy.<clinit>
// init-at-new L8W98SerialCtorShapes$Lazy log=
// eiie java.lang.ExceptionInInitializerError cause=java.lang.IllegalStateException: boom
// eiie-again java.lang.NoClassDefFoundError: Could not initialize class L8W98SerialCtorShapes$Boom
// argcount java.lang.IllegalArgumentException: wrong number of arguments: 1 expected: 0
// one-arg-private null
// one-arg-package L8W98SerialCtorShapes$PkgSer declaring=L8W98SerialCtorShapes$PkgBase log=PkgBase()
// HOTSPOT_EXPECTED_END
import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationTargetException;
import java.io.Serializable;
import java.util.ArrayList;
import java.util.List;
import sun.reflect.ReflectionFactory;

public class L8W98SerialCtorShapes {
    static final List<String> LOG = new ArrayList<>();

    static String drain() {
        String s = String.join(" ", LOG);
        LOG.clear();
        return s;
    }

    static class Plain {
        int v = 5;

        Plain() {
            LOG.add("Plain()");
        }
    }

    abstract static class Abs {
        int a = 1;
    }

    static class Lazy {
        static {
            LOG.add("Lazy.<clinit>");
        }
    }

    static class Boom {
        static final int X = fail();

        static int fail() {
            throw new IllegalStateException("boom");
        }
    }

    static class Base2 {
        Base2() {
            LOG.add("Base2()");
        }
    }

    static class Leaf extends Base2 {
        int w = 9;

        Leaf() {
            LOG.add("Leaf()");
        }
    }

    static class PrivBase {
        private PrivBase() {
        }

        PrivBase(int x) {
        }
    }

    static class PrivSer extends PrivBase implements Serializable {
        PrivSer() {
            super(1);
        }
    }

    static class PkgBase {
        PkgBase() {
            LOG.add("PkgBase()");
        }
    }

    static class PkgSer extends PkgBase implements Serializable {
        PkgSer() {
            LOG.add("PkgSer()");
        }
    }

    static String outcome(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    @SuppressWarnings("deprecation")
    public static void main(String[] args) throws Exception {
        ReflectionFactory rf = ReflectionFactory.getReflectionFactory();
        Constructor<Object> objCtor = Object.class.getDeclaredConstructor();

        Constructor<?> c = rf.newConstructorForSerialization(Plain.class, objCtor);
        Object o = c.newInstance();
        System.out.println("plain " + o.getClass().getName() + " v=" + ((Plain) o).v + " log=" + drain());
        System.out.println("plain-ctor " + c.getDeclaringClass().getName() + " params="
                + c.getParameterCount() + " accessible=" + c.isAccessible());

        Object leaf = rf.newConstructorForSerialization(Leaf.class, Base2.class.getDeclaredConstructor())
                .newInstance();
        System.out.println("ancestor " + leaf.getClass().getName() + " w=" + ((Leaf) leaf).w + " log=" + drain());

        Constructor<Plain> own = Plain.class.getDeclaredConstructor();
        Constructor<?> same = rf.newConstructorForSerialization(Plain.class, own);
        Object p = same.newInstance();
        System.out.println("same-class " + (same == own) + " " + p.getClass().getName() + " v="
                + ((Plain) p).v + " log=" + drain());

        try {
            rf.newConstructorForSerialization(Plain.class, String.class.getConstructor());
            System.out.println("unrelated no-throw");
        } catch (Throwable t) {
            System.out.println("unrelated " + outcome(t));
        }

        try {
            Object a = rf.newConstructorForSerialization(Abs.class, objCtor).newInstance();
            System.out.println("abstract no-throw " + a.getClass().getName());
        } catch (InvocationTargetException e) {
            System.out.println("abstract " + e.getClass().getName() + " cause=" + outcome(e.getCause()));
        } catch (Throwable t) {
            System.out.println("abstract " + outcome(t));
        }

        Constructor<?> lazy = rf.newConstructorForSerialization(Lazy.class, objCtor);
        System.out.println("init-at-create " + drain());
        Object l = lazy.newInstance();
        System.out.println("init-at-new " + l.getClass().getName() + " log=" + drain());

        try {
            rf.newConstructorForSerialization(Boom.class, objCtor);
            System.out.println("eiie no-throw");
        } catch (ExceptionInInitializerError e) {
            System.out.println("eiie " + e.getClass().getName() + " cause=" + outcome(e.getCause()));
        } catch (Throwable t) {
            System.out.println("eiie " + outcome(t));
        }
        try {
            rf.newConstructorForSerialization(Boom.class, objCtor);
            System.out.println("eiie-again no-throw");
        } catch (Throwable t) {
            System.out.println("eiie-again " + outcome(t));
        }

        try {
            Object x = rf.newConstructorForSerialization(Plain.class, objCtor).newInstance(1);
            System.out.println("argcount no-throw " + x.getClass().getName());
        } catch (Throwable t) {
            System.out.println("argcount " + outcome(t));
        }

        System.out.println("one-arg-private " + rf.newConstructorForSerialization(PrivSer.class));
        Constructor<?> pkg = rf.newConstructorForSerialization(PkgSer.class);
        Object ps = pkg.newInstance();
        System.out.println("one-arg-package " + ps.getClass().getName() + " declaring="
                + pkg.getDeclaringClass().getName() + " log=" + drain());
    }
}
