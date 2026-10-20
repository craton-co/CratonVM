// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L9 -- Throwable.getStackTrace() frames
// for exceptions raised in the places a VM has to splice frames together:
//
//   lambda        inside a lambda body run through a functional interface
//                 (the hidden proxy frame is omitted)
//   method-ref    a static method reference
//   reflect       Method.invoke (JDK 25: DirectMethodHandleAccessor frames)
//   reflect-ctor  Constructor.newInstance (DirectConstructorHandleAccessor)
//   handle        MethodHandle.invokeExact (LambdaForm frames hidden)
//   clinit        a static initializer (<clinit>) reached through getstatic
//   init          a constructor (<init>) and a super constructor
//   native-copy   System.arraycopy (a "Native Method" top frame)
//   native-array  Array.newInstance (Array.newArray native frame)
//   native-clone  Object.clone of a non-Cloneable (Object.clone native frame)
//   thread        an exception in a started thread (Thread.run / runWith)
//   getters       StackTraceElement getters for app / java.base frames
//
// Every frame prints with StackTraceElement.toString() (line numbers kept:
// both VMs run the same JDK class files), up to the probe's own main frame.
//
//   $J/javac -d out tools/probes/interp/L9/L9W98StackFrameShapes.java
//   $B [--compatible] [--nojit] --java-home $J/.. -cp out L9W98StackFrameShapes
//   $J/java -cp out L9W98StackFrameShapes
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Array;
import java.lang.reflect.InvocationTargetException;
import java.util.function.Supplier;

public class L9W98StackFrameShapes {
    static class Boom {
        static int V = explode();

        static int explode() {
            throw new IllegalStateException("clinit");
        }
    }

    static class Base {
        Base(boolean fail) {
            if (fail) throw new IllegalStateException("base ctor");
        }
    }

    static class Derived extends Base {
        Derived() {
            super(true);
        }
    }

    static class NoClone {
        Object copy() throws CloneNotSupportedException {
            return clone();
        }
    }

    public L9W98StackFrameShapes() {
        throw new IllegalStateException("reflective ctor");
    }

    public static int target() {
        throw new IllegalStateException("reflective");
    }

    static int viaRef() {
        throw new IllegalStateException("method ref");
    }

    interface Thrower {
        void run() throws Throwable;
    }

    static Throwable caught(Thrower t) {
        try {
            t.run();
        } catch (Throwable e) {
            return e;
        }
        throw new AssertionError("nothing thrown");
    }

    static void show(String label, Throwable t) {
        System.out.println("== " + label + ": " + t);
        for (StackTraceElement e : t.getStackTrace()) {
            String s = e.toString().replaceAll("\\$\\$Lambda/0x[0-9a-f]+", "\\$\\$Lambda/0xN");
            System.out.println("  " + s);
            if (e.getClassName().equals("L9W98StackFrameShapes") && e.getMethodName().equals("main")) {
                break;
            }
        }
    }

    public static void main(String[] args) throws Throwable {
        Supplier<Integer> lambda = () -> {
            throw new IllegalStateException("lambda");
        };
        show("lambda", caught(lambda::get));

        Supplier<Integer> ref = L9W98StackFrameShapes::viaRef;
        show("method-ref", caught(ref::get));

        Throwable reflect = caught(() -> L9W98StackFrameShapes.class.getMethod("target").invoke(null));
        show("reflect", reflect);
        show("reflect cause", reflect.getCause());

        Throwable ctor = caught(() -> L9W98StackFrameShapes.class.getConstructor().newInstance());
        show("reflect-ctor", ctor);
        show("reflect-ctor cause", ctor.getCause());

        MethodHandle mh = MethodHandles.lookup()
                .findStatic(L9W98StackFrameShapes.class, "target", MethodType.methodType(int.class));
        show("handle", caught(() -> {
            int r = (int) mh.invokeExact();
        }));

        Throwable clinit = caught(() -> System.out.println(Boom.V));
        show("clinit", clinit);
        show("clinit cause", clinit.getCause());

        show("init", caught(Derived::new));

        show("native-copy", caught(() -> System.arraycopy(new int[1], 0, new long[1], 0, 1)));
        show("native-array", caught(() -> Array.newInstance(int.class, -1)));
        show("native-clone", caught(() -> new NoClone().copy()));

        Throwable[] inThread = new Throwable[1];
        Thread t = new Thread(() -> {
            try {
                target();
            } catch (Throwable e) {
                inThread[0] = e;
            }
        }, "frames-worker");
        t.start();
        t.join();
        StackTraceElement[] threadFrames = inThread[0].getStackTrace();
        System.out.println("== thread: " + inThread[0]);
        for (StackTraceElement e : threadFrames) {
            System.out.println("  " + e);
        }

        StackTraceElement app = caught(L9W98StackFrameShapes::target).getStackTrace()[0];
        System.out.println("== getters app: loader=" + app.getClassLoaderName()
                + " module=" + app.getModuleName() + " version=" + app.getModuleVersion()
                + " native=" + app.isNativeMethod() + " file=" + app.getFileName());
        StackTraceElement base = caught(() -> System.arraycopy(null, 0, null, 0, 0)).getStackTrace()[0];
        System.out.println("== getters base: loader=" + base.getClassLoaderName()
                + " module=" + base.getModuleName()
                + " version-is-runtime=" + Runtime.version().toString().startsWith(String.valueOf(base.getModuleVersion()))
                + " native=" + base.isNativeMethod() + " line=" + base.getLineNumber()
                + " file=" + base.getFileName());
    }
}
