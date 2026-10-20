// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 62, lane L1: `Instrumentation.getObjectSize` of
// instance and array shapes against HotSpot, item 1 of
// docs/internal/fixed-bugs/interpreter-L1-get-object-size-answers-this-vms-footprint-not-hotspots-FIXED-20261026.md
// (the C table's `GetObjectSize` answers the same number,
// `runtime::instrument::object_size_of`). Each row prints the size, then the
// first row again at the end (the cold-VM first-call rule).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L1W62GetObjectSizeShapes$Agent
// containing L1W62GetObjectSizeShapes*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L1W62GetObjectSizeShapes
// Without the agent both VMs print "no agent".
//
// HotSpot 25 prints (agent; `java`, default flags, so compressed references
// and compressed class pointers; MEASURED, JDK 25.0.3, three runs):
//
//   size Object[3]: 32
//   size Object[0]: 16
//   size Object[1]: 24
//   size Object[2]: 24
//   size String[5]: 40
//   size int[][] of 3: 32
//   size Empty: 16
//   size OneRef: 16
//   size TwoRefs: 24
//   size ThreeRefs: 24
//   size OneInt: 16
//   size OneLong: 24
//   size LongAndRef: 24
//   size ByteAndRef: 24
//   size IntSub: 24
//   size RefSub: 24
//   size Mixed: 48
//   size ArrayList: 24
//   size HashMap: 48
//   size String: 24
//   size Throwable: 40
//   size InternalError: 40
//   size URLClassLoader: 96
//   size Module: 56
//   size Thread: 112
//   size Object.class: 120
//   size Empty.class: 120
//   size StaticInt.class: 128
//   size StaticRef.class: 128
//   size StaticLong.class: 128
//   size StaticByteAndLong.class: 136
//   size StaticMixed.class: 144
//   size String.class: 144
//   size int.class: 120
//   size Object[].class: 120
//   size Object[3]: 32
//
// CratonVM before wave 62 (and with `CRATONVM_HOTSPOT_OBJECT_SIZE=0`, the
// negative control) answers this heap's footprint: 8-byte references
// (`Object[3]: 40`, `ThreeRefs: 32`, `HashMap: 56`), a smaller header
// (`OneLong: 16`, `IntSub: 16`) and its own mirrors (`Object.class: 144`).
// With the switch on (wave 62, lane L1;
// `runtime::instrument::hotspot_object_size`): HotSpot's lines, both modes.
import java.lang.instrument.Instrumentation;

public class L1W62GetObjectSizeShapes {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    static class Empty {
    }

    static class OneRef {
        Object a;
    }

    static class TwoRefs {
        Object a;
        Object b;
    }

    static class ThreeRefs {
        Object a;
        Object b;
        Object c;
    }

    static class OneInt {
        int a;
    }

    static class OneLong {
        long a;
    }

    static class LongAndRef {
        long a;
        Object b;
    }

    static class ByteAndRef {
        byte a;
        Object b;
    }

    static class IntSub extends OneInt {
        int b;
    }

    static class RefSub extends OneRef {
        Object b;
    }

    static class Mixed {
        boolean z;
        byte b;
        char c;
        short s;
        int i;
        float f;
        long j;
        double d;
        Object o;
    }

    static class StaticInt {
        static int a;
    }

    static class StaticRef {
        static Object a;
    }

    static class StaticLong {
        static long a;
    }

    static class StaticByteAndLong {
        static byte a;
        static long b;
    }

    static class StaticMixed {
        static Object a;
        static int b;
        static Object c;
        static short d;
        static double e;
        int instanceField;
    }

    static void row(String label, Object o) {
        System.out.println("size " + label + ": " + inst.getObjectSize(o));
    }

    public static void main(String[] args) {
        if (inst == null) {
            System.out.println("no agent");
            return;
        }
        row("Object[3]", new Object[3]);
        row("Object[0]", new Object[0]);
        row("Object[1]", new Object[1]);
        row("Object[2]", new Object[2]);
        row("String[5]", new String[5]);
        row("int[][] of 3", new int[3][]);
        row("Empty", new Empty());
        row("OneRef", new OneRef());
        row("TwoRefs", new TwoRefs());
        row("ThreeRefs", new ThreeRefs());
        row("OneInt", new OneInt());
        row("OneLong", new OneLong());
        row("LongAndRef", new LongAndRef());
        row("ByteAndRef", new ByteAndRef());
        row("IntSub", new IntSub());
        row("RefSub", new RefSub());
        row("Mixed", new Mixed());
        row("ArrayList", new java.util.ArrayList<Object>());
        row("HashMap", new java.util.HashMap<Object, Object>());
        row("String", "abc");
        row("Throwable", new Throwable());
        row("InternalError", new InternalError());
        row("URLClassLoader", new java.net.URLClassLoader(new java.net.URL[0]));
        row("Module", Object.class.getModule());
        row("Thread", new Thread());
        row("Object.class", Object.class);
        row("Empty.class", Empty.class);
        row("StaticInt.class", StaticInt.class);
        row("StaticRef.class", StaticRef.class);
        row("StaticLong.class", StaticLong.class);
        row("StaticByteAndLong.class", StaticByteAndLong.class);
        row("StaticMixed.class", StaticMixed.class);
        row("String.class", String.class);
        row("int.class", int.class);
        row("Object[].class", Object[].class);
        row("Object[3]", new Object[3]);
    }
}
