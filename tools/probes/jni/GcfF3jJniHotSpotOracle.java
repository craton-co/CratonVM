// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// gcf f3/j (2026-10-01): reads HotSpot's OWN answers to JNI edge cases with no C compiler:
// RegisterNatives binds a native method to an FFM upcall stub whose Java body calls the
// JNI table through JNIEnv (handles live in that native frame; class / object refs are
// promoted to globals, as plain locals did not survive between downcalls). A pending
// exception surfaces as a Java throw at the downcall's return, so each call is wrapped.
// Run on a JDK only (it is the oracle, not a CratonVM probe):
//   javac -d cls GcfF3jJniHotSpotOracle.java
//   java --enable-native-access=ALL-UNNAMED -XX:+UseSerialGC -Xbootclasspath/a:cls -cp cls GcfF3jJniHotSpotOracle
// (Windows: jvm.dll under bin/server; edit the path for libjvm.so on Linux.)
import java.lang.foreign.*;
import java.lang.invoke.*;
import java.nio.file.*;
import static java.lang.foreign.ValueLayout.*;

/**
 * HotSpot's own JNI answers, read through a native method whose body is an FFM
 * upcall: RegisterNatives binds probe() to the upcall stub, so every JNI call
 * the body makes runs inside a real JNI native frame (its locals live there).
 */
public class GcfF3jJniHotSpotOracle {
    static final Linker L = Linker.nativeLinker();
    static final Arena AR = Arena.global();
    static MemorySegment table;

    static native void probe();

    static int instanceInt = 2;

    static MethodHandle h(int idx, FunctionDescriptor fd) {
        return L.downcallHandle(table.getAtIndex(ADDRESS, idx), fd);
    }

    static MemorySegment c(String s) {
        return AR.allocateFrom(s);
    }

    static final FunctionDescriptor ID4 = FunctionDescriptor.of(ADDRESS, ADDRESS, ADDRESS, ADDRESS, ADDRESS);

    /** The upcall body: (JNIEnv*, jclass). Must not throw. */
    static void body(MemorySegment envp, MemorySegment self) {
        try {
            MemorySegment env = envp.reinterpret(8);
            table = env.get(ADDRESS, 0).reinterpret(8 * 240);
            MethodHandle findClass = h(6, FunctionDescriptor.of(ADDRESS, ADDRESS, ADDRESS));
            MethodHandle occurred = h(15, FunctionDescriptor.of(ADDRESS, ADDRESS));
            MethodHandle clear = h(17, FunctionDescriptor.ofVoid(ADDRESS));
            MethodHandle getObjectClass = h(31, FunctionDescriptor.of(ADDRESS, ADDRESS, ADDRESS));
            MethodHandle callObj = h(36, FunctionDescriptor.of(ADDRESS, ADDRESS, ADDRESS, ADDRESS, ADDRESS));
            MethodHandle getMethodID = h(33, ID4);
            MethodHandle getFieldID = h(94, ID4);
            MethodHandle getStaticMethodID = h(113, ID4);
            MethodHandle getStaticFieldID = h(144, ID4);
            MethodHandle newStringUTF = h(167, FunctionDescriptor.of(ADDRESS, ADDRESS, ADDRESS));
            MethodHandle getStringRegion = h(220, FunctionDescriptor.ofVoid(ADDRESS, ADDRESS, JAVA_INT, JAVA_INT, ADDRESS));
            MethodHandle getStringUTFRegion = h(221, FunctionDescriptor.ofVoid(ADDRESS, ADDRESS, JAVA_INT, JAVA_INT, ADDRESS));
            MethodHandle getStringUTFChars = h(169, FunctionDescriptor.of(ADDRESS, ADDRESS, ADDRESS, ADDRESS));
            MethodHandle newIntArray = h(179, FunctionDescriptor.of(ADDRESS, ADDRESS, JAVA_INT));
            MethodHandle getIntArrayRegion = h(203, FunctionDescriptor.ofVoid(ADDRESS, ADDRESS, JAVA_INT, JAVA_INT, ADDRESS));

            MemorySegment throwableClass = (MemorySegment) findClass.invokeExact(env, c("java/lang/Throwable"));
            MemorySegment toStr = (MemorySegment) getMethodID.invokeExact(env, throwableClass, c("toString"), c("()Ljava/lang/String;"));

            // Prints the pending exception's toString() and clears it.
            Pend pend = what -> {
                MemorySegment t = (MemorySegment) occurred.invokeExact(env);
                if (t.address() == 0) {
                    System.out.println(what + ": none");
                    return;
                }
                clear.invokeExact(env);
                MemorySegment s = (MemorySegment) callObj.invokeExact(env, t, toStr, MemorySegment.NULL);
                MemorySegment chars = (MemorySegment) getStringUTFChars.invokeExact(env, s, MemorySegment.NULL);
                System.out.println(what + ": " + chars.reinterpret(1000).getString(0));
            };

            MethodHandle ngr = h(21, FunctionDescriptor.of(ADDRESS, ADDRESS, ADDRESS));
            MemorySegment str = (MemorySegment) ngr.invokeExact(env, (MemorySegment) findClass.invokeExact(env, c("java/lang/String")));
            MemorySegment integer = (MemorySegment) ngr.invokeExact(env, (MemorySegment) findClass.invokeExact(env, c("java/lang/Integer")));
            MemorySegment runnable = (MemorySegment) ngr.invokeExact(env, (MemorySegment) findClass.invokeExact(env, c("java/lang/Runnable")));
            MemorySegment m;
            try { m = (MemorySegment) getMethodID.invokeExact(env, str, c("nope"), c("()V")); System.out.println("  -> id null=" + (m.address() == 0)); } catch (Throwable t) { System.out.println("  -> threw " + t); }
            pend.p("GetMethodID String.nope()V");
            try { m = (MemorySegment) getStaticMethodID.invokeExact(env, str, c("nope"), c("()V")); System.out.println("  -> id null=" + (m.address() == 0)); } catch (Throwable t) { System.out.println("  -> threw " + t); }
            pend.p("GetStaticMethodID String.nope()V");
            try { m = (MemorySegment) getStaticMethodID.invokeExact(env, str, c("length"), c("()I")); System.out.println("  -> id null=" + (m.address() == 0)); } catch (Throwable t) { System.out.println("  -> threw " + t); }
            pend.p("GetStaticMethodID String.length (instance)");
            try { m = (MemorySegment) getMethodID.invokeExact(env, str, c("valueOf"), c("(I)Ljava/lang/String;")); System.out.println("  -> id null=" + (m.address() == 0)); } catch (Throwable t) { System.out.println("  -> threw " + t); }
            pend.p("GetMethodID String.valueOf(I) (static)");
            try { m = (MemorySegment) getMethodID.invokeExact(env, runnable, c("nope"), c("()V")); System.out.println("  -> id null=" + (m.address() == 0)); } catch (Throwable t) { System.out.println("  -> threw " + t); }
            pend.p("GetMethodID Runnable.nope()V");
            try { m = (MemorySegment) getFieldID.invokeExact(env, integer, c("nope"), c("I")); System.out.println("  -> id null=" + (m.address() == 0)); } catch (Throwable t) { System.out.println("  -> threw " + t); }
            pend.p("GetFieldID Integer.nope I");
            try { m = (MemorySegment) getFieldID.invokeExact(env, integer, c("MAX_VALUE"), c("I")); System.out.println("  -> id null=" + (m.address() == 0)); } catch (Throwable t) { System.out.println("  -> threw " + t); }
            pend.p("GetFieldID Integer.MAX_VALUE (static)");
            try { m = (MemorySegment) getStaticFieldID.invokeExact(env, integer, c("value"), c("I")); System.out.println("  -> id null=" + (m.address() == 0)); } catch (Throwable t) { System.out.println("  -> threw " + t); }
            pend.p("GetStaticFieldID Integer.value (instance)");
            try { m = (MemorySegment) getStaticFieldID.invokeExact(env, integer, c("nope"), c("I")); System.out.println("  -> id null=" + (m.address() == 0)); } catch (Throwable t) { System.out.println("  -> threw " + t); }
            pend.p("GetStaticFieldID Integer.nope");

            // A lambda's class.
            Runnable lam = () -> { };
            sink = lam;

            MemorySegment s = (MemorySegment) ngr.invokeExact(env, (MemorySegment) newStringUTF.invokeExact(env, c("abcdefgh")));
            MemorySegment buf = AR.allocate(64);
            buf.fill((byte) 'X');
            try { getStringUTFRegion.invokeExact(env, s, 2, 3, buf); System.out.println("  -> ok"); } catch (Throwable t) { System.out.println("  -> threw " + t); }
            System.out.println("utf-tail-3: " + (buf.get(JAVA_BYTE, 3) & 0xff));
            pend.p("utf-region 2,3");
            buf.fill((byte) 'X');
            try { getStringUTFRegion.invokeExact(env, s, 2, 0, buf); System.out.println("  -> ok"); } catch (Throwable t) { System.out.println("  -> threw " + t); }
            System.out.println("utf-tail-0: " + (buf.get(JAVA_BYTE, 0) & 0xff));
            pend.p("utf-region 2,0");
            try { getStringUTFRegion.invokeExact(env, s, 6, 3, buf); System.out.println("  -> ok"); } catch (Throwable t) { System.out.println("  -> threw " + t); }
            pend.p("utf-region past-end");
            try { getStringUTFRegion.invokeExact(env, s, -1, 2, buf); System.out.println("  -> ok"); } catch (Throwable t) { System.out.println("  -> threw " + t); }
            pend.p("utf-region neg-start");
            try { getStringRegion.invokeExact(env, s, 6, 3, buf); System.out.println("  -> ok"); } catch (Throwable t) { System.out.println("  -> threw " + t); }
            pend.p("string-region past-end");
            try { getStringRegion.invokeExact(env, s, -1, 2, buf); System.out.println("  -> ok"); } catch (Throwable t) { System.out.println("  -> threw " + t); }
            pend.p("string-region neg-start");
            try { getStringRegion.invokeExact(env, s, 0, -1, buf); System.out.println("  -> ok"); } catch (Throwable t) { System.out.println("  -> threw " + t); }
            pend.p("string-region neg-len");
            MemorySegment arr = (MemorySegment) ngr.invokeExact(env, (MemorySegment) newIntArray.invokeExact(env, 4));
            try { getIntArrayRegion.invokeExact(env, arr, 2, 3, buf); System.out.println("  -> ok"); } catch (Throwable t) { System.out.println("  -> threw " + t); }
            pend.p("int-region past-end");
            try { getIntArrayRegion.invokeExact(env, arr, -1, 2, buf); System.out.println("  -> ok"); } catch (Throwable t) { System.out.println("  -> threw " + t); }
            pend.p("int-region neg-start");
            try { getIntArrayRegion.invokeExact(env, arr, 0, -1, buf); System.out.println("  -> ok"); } catch (Throwable t) { System.out.println("  -> threw " + t); }
            pend.p("int-region neg-len");
            try { getIntArrayRegion.invokeExact(env, arr, 4, 0, buf); System.out.println("  -> ok"); } catch (Throwable t) { System.out.println("  -> threw " + t); }
            pend.p("int-region empty-at-end");
        } catch (Throwable t) {
            System.out.println("body threw " + t);
        }
    }

    static volatile Object sink;

    interface Pend {
        void p(String what) throws Throwable;
    }

    public static void main(String[] args) throws Throwable {
        SymbolLookup jvm = SymbolLookup.libraryLookup(
                Path.of(System.getProperty("java.home"), "bin", "server", "jvm.dll"), AR);
        MethodHandle created = L.downcallHandle(jvm.find("JNI_GetCreatedJavaVMs").get(),
                FunctionDescriptor.of(JAVA_INT, ADDRESS, JAVA_INT, ADDRESS));
        MemorySegment vmbuf = AR.allocate(ADDRESS);
        MemorySegment n = AR.allocate(JAVA_INT);
        int rc0 = (int) created.invokeExact(vmbuf, 1, n);
        MemorySegment vm = vmbuf.get(ADDRESS, 0).reinterpret(8);
        MemorySegment vtab = vm.get(ADDRESS, 0).reinterpret(8 * 16);
        MethodHandle getEnv = L.downcallHandle(vtab.getAtIndex(ADDRESS, 6),
                FunctionDescriptor.of(JAVA_INT, ADDRESS, ADDRESS, JAVA_INT));
        MemorySegment envbuf = AR.allocate(ADDRESS);
        int rc = (int) getEnv.invokeExact(vm, envbuf, 0x00180000);
        MemorySegment env = envbuf.get(ADDRESS, 0).reinterpret(8);
        table = env.get(ADDRESS, 0).reinterpret(8 * 240);

        MethodHandle target = MethodHandles.lookup().findStatic(GcfF3jJniHotSpotOracle.class, "body",
                MethodType.methodType(void.class, MemorySegment.class, MemorySegment.class));
        MemorySegment stub = L.upcallStub(target, FunctionDescriptor.ofVoid(ADDRESS, ADDRESS), AR);
        // JNINativeMethod { char* name; char* signature; void* fnPtr; }
        MemorySegment nm = AR.allocate(24);
        nm.set(ADDRESS, 0, c("probe"));
        nm.set(ADDRESS, 8, c("()V"));
        nm.set(ADDRESS, 16, stub);
        MethodHandle findClass = h(6, FunctionDescriptor.of(ADDRESS, ADDRESS, ADDRESS));
        MethodHandle register = h(215, FunctionDescriptor.of(JAVA_INT, ADDRESS, ADDRESS, ADDRESS, JAVA_INT));
        // FindClass's local is used in the very next call only.
        MemorySegment me = (MemorySegment) findClass.invokeExact(env, c("GcfF3jJniHotSpotOracle"));
        int r = (int) register.invokeExact(env, me, nm, 1);
        System.out.println("register: " + r);
        probe();
    }
}
