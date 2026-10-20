// Interpreter round i3 wave 83, lane L1: JVMTI `NativeMethodBind` of the
// JDK's own natives, enabled in `Agent_OnLoad`, against HotSpot
// (docs/internal/fixed-bugs/interpreter-i63-L1-natives-this-vm-implements-post-no-native-method-bind-FIXED-20261115.md).
//
// The startup agent enables `NativeMethodBind` before any Java code runs and
// records every bind; `main` calls four JDK natives this VM serves from Rust
// (`Object.hashCode`, `System.arraycopy`, `System.nanoTime`,
// `Thread.currentThread`) and then the shim's own native `rows`, which
// answers how many binds each of those five methods saw, whether the
// address was non-NULL, and whether the JDK's natives were bound at all.
// Order-, address- and count-insensitive by construction (see the shim).
//
// Needs the native shim tools/probes/interp/L1/L1W83NativeMethodBind.c,
// loaded both as an agent and with `System.load` (for the native):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w83bind.so tools/probes/interp/L1/L1W83NativeMethodBind.c
//   javac -d /tmp/l1w83bind tools/probes/interp/L1/L1W83NativeMethodBind.java
//   java     -agentpath:/tmp/libl1w83bind.so -cp /tmp/l1w83bind L1W83NativeMethodBind /tmp/libl1w83bind.so
//   cratonvm -agentpath:/tmp/libl1w83bind.so -cp /tmp/l1w83bind L1W83NativeMethodBind /tmp/libl1w83bind.so
//   cratonvm --compatible -agentpath:/tmp/libl1w83bind.so -cp /tmp/l1w83bind L1W83NativeMethodBind /tmp/libl1w83bind.so
//
// (a `cratonvm` built with `--features experimental-debug`, as every C
// JVMTI probe needs). Without the argument it prints only the usage line.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// java: hash=ok copy=3 time=ok thread=main
// bind Ljava/lang/Object;.hashCode()I: 1 address=nonnull
// bind Ljava/lang/System;.arraycopy(Ljava/lang/Object;ILjava/lang/Object;II)V: 1 address=nonnull
// bind Ljava/lang/System;.nanoTime()J: 1 address=nonnull
// bind Ljava/lang/Thread;.currentThread()Ljava/lang/Thread;: 1 address=nonnull
// bind LL1W83NativeMethodBind;.rows()Ljava/lang/String;: 1 address=nonnull
// jdk natives bound: at least 10
// every bind named: yes
// HOTSPOT_EXPECTED_END
//
// The block is derived, not measured (refresh it from `$J/java` first): JDK
// 25 binds `System`'s and `Thread`'s natives in their `registerNatives`
// (`System.c`, `Thread.c`; JVMTI posts a bind per `RegisterNatives` entry),
// `Object.hashCode` in `java_lang_Object::register_natives` at VM start
// (`Method::register_native`, which posts the bind too), and the shim's
// `rows` at its first call (a library symbol). Each once.
//
// CratonVM before wave 83: the four JDK rows `0 address=-` and `jdk natives
// bound: none` (a registered Rust native had no bind); `rows` 1 (wave 63's
// library-symbol bind). Wave 83: the funnel posts each registered native's
// bind the first time it runs, when the event was enabled in
// `Agent_OnLoad`.
public class L1W83NativeMethodBind {
    static native String rows();

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W83NativeMethodBind <path of the shim library>");
            return;
        }
        System.load(args[0]);
        Object o = new Object();
        int h = o.hashCode();
        int[] from = {1, 2, 3};
        int[] to = new int[3];
        System.arraycopy(from, 0, to, 0, 3);
        long t = System.nanoTime();
        String thread = Thread.currentThread().getName();
        System.out.println("java: hash=" + (h == o.hashCode() ? "ok" : "differs")
                + " copy=" + to[2]
                + " time=" + (t != 0 ? "ok" : "zero")
                + " thread=" + thread);
        System.out.print(rows());
    }
}
