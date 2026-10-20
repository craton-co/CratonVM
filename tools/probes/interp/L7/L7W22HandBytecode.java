/*
 * Interpreter round i1, wave 22, lane L7: JVMS shapes javac never emits, run
 * from hand-assembled class files embedded below (base64). HotSpot comparison
 * probe for the interpreter core.
 *
 *   mon     unstructured locking (JVMS §2.11.10): a method that returns, or
 *           throws, while holding a monitor it entered; a `monitorexit` with
 *           no `monitorenter`; a callee exiting its CALLER's monitor
 *           (`callerThenCalleeExits`: enter o; exitOnly(o); reached = 1;
 *           exit o; return — no handler).
 *   jsr     `jsr` / `ret` in a version-49 class, `wide astore` + `wide ret`,
 *           and a `finally` compiled to a subroutine, on both exits.
 *   switch  `tableswitch` with low = MIN_VALUE and high = MAX_VALUE,
 *           `lookupswitch` with MIN_VALUE / -1 / 0 / MAX_VALUE keys, each at
 *           four bcis (padding 3, 2, 1, 0).
 *   wide    `wide` iload/lload/fload/dload/aload/istore/.../iinc (index >=
 *           256, iinc constants -30000 and 32767), narrow iinc -128 / 127.
 *   exc     exception-table edges: a throw AT end_pc is not covered (end is
 *           exclusive), a range starting AT the athrow covers it, a handler
 *           that is the start of its own range re-enters it, and the first
 *           matching row wins among overlapping rows.
 *
 * Run on CratonVM with `--nojit` (the interpreter), and without it; both
 * modes. HotSpot 25 (`-Xint` and default) prints exactly:
 *
 *   mon enterOnly threw java.lang.IllegalMonitorStateException: null
 *   mon enterOnly holdsLock false
 *   mon exitOnly threw java.lang.IllegalMonitorStateException: null
 *   mon exitOnly holdsLock false
 *   mon enterTwiceExitOnce threw java.lang.IllegalMonitorStateException: null
 *   mon enterTwiceExitOnce holdsLock false
 *   mon enterThenThrow threw java.lang.IllegalMonitorStateException: null
 *   mon enterThenThrow holdsLock false
 *   mon enterReturnInt threw java.lang.IllegalMonitorStateException: null
 *   mon enterReturnInt holdsLock false
 *   mon callerThenCalleeExits threw java.lang.IllegalMonitorStateException: null
 *   mon callerThenCalleeExits holdsLock false
 *   mon reached 0
 *   jsr twice 25 wideRet 0 finally 108
 *   jsr finally threw java.lang.RuntimeException: neg
 *   switch pad0 tsMin:1,2,3,-1,-1,-1,-1,-1,-1,-1,-1, tsMax:-1,-1,-1,-1,-1,-1,-1,-1,1,2,3, ls:1,-1,-1,-1,2,3,-1,-1,-1,-1,4,
 *   switch pad1 tsMin:1,2,3,-1,-1,-1,-1,-1,-1,-1,-1, tsMax:-1,-1,-1,-1,-1,-1,-1,-1,1,2,3, ls:1,-1,-1,-1,2,3,-1,-1,-1,-1,4,
 *   switch pad2 tsMin:1,2,3,-1,-1,-1,-1,-1,-1,-1,-1, tsMax:-1,-1,-1,-1,-1,-1,-1,-1,1,2,3, ls:1,-1,-1,-1,2,3,-1,-1,-1,-1,4,
 *   switch pad3 tsMin:1,2,3,-1,-1,-1,-1,-1,-1,-1,-1, tsMax:-1,-1,-1,-1,-1,-1,-1,-1,1,2,3, ls:1,-1,-1,-1,2,3,-1,-1,-1,-1,4,
 *   wide I 2772 J -1 F -1.5 D -0.0 A s iinc 8
 *   exc endExclusive threw java.lang.NullPointerException
 *   exc startInclusive -> 43
 *   exc selfHandler -> 3
 *   exc firstMatch -> 2
 *
 * CratonVM before any fix (read from the code, wave 22; the orchestrator's run
 * is the record): the `mon` rows diverge. The interpreter keeps no per-frame
 * monitor record, so `enterOnly` / `enterTwiceExitOnce` / `enterReturnInt`
 * return normally with the lock still held (`holdsLock true`),
 * `enterThenThrow` propagates its RuntimeException with the lock held,
 * `exitOnly`'s message is "thread N does not own the monitor for object at
 * 0x..." instead of null, and `callerThenCalleeExits` reaches `reached = 1`
 * (the callee's exit of the caller's monitor succeeds) and throws from the
 * caller's own `monitorexit`. See
 * docs/internal/fixed-bugs/interpreter-L7-unstructured-locking-is-not-detected-FIXED-20260926.md.
 * Every other row is expected to match. Since wave 23 (lane L7, the per-frame
 * `Frame::held_monitors` record) every row, `mon` included, must match.
 *
 * The class files were assembled on JDK 25 with java.lang.classfile; the
 * generator is not needed to run the probe.
 */
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.Base64;

public class L7W22HandBytecode {
    // W22M: version 69, monitor shapes.
    static final String M_B64 = "yv66vgAAAEUAHAEABFcyMk0HAAEBABBqYXZhL2xhbmcvT2JqZWN0BwADAQAJZW50ZXJPbmx5AQAVKExqYXZhL2xh" +
            "bmcvT2JqZWN0OylWAQAIZXhpdE9ubHkBABJlbnRlclR3aWNlRXhpdE9uY2UBAA5lbnRlclRoZW5UaHJvdwEAGmph" +
            "dmEvbGFuZy9SdW50aW1lRXhjZXB0aW9uBwAKAQAEYm9vbQgADAEABjxpbml0PgEAFShMamF2YS9sYW5nL1N0cmlu" +
            "ZzspVgwADgAPCgALABABAAdyZWFjaGVkAQABSQEAFWNhbGxlclRoZW5DYWxsZWVFeGl0cwwABwAGCgACABUMABIA" +
            "EwkAAgAXAQAOZW50ZXJSZXR1cm5JbnQBABUoTGphdmEvbGFuZy9PYmplY3Q7KUkBAARDb2RlACEAAgAEAAAAAQAJ" +
            "ABIAEwAAAAYACQAFAAYAAQAbAAAADwABAAEAAAADKsKxAAAAAAAJAAcABgABABsAAAAPAAEAAQAAAAMqw7EAAAAA" +
            "AAkACAAGAAEAGwAAABMAAQABAAAAByrCKsIqw7EAAAAAAAkACQAGAAEAGwAAABgAAwABAAAADCrCuwALWRINtwAR" +
            "vwAAAAAACQAUAAYAAQAbAAAAGQABAAEAAAANKsIquAAWBLMAGCrDsQAAAAAACQAZABoAAQAbAAAAEQABAAEAAAAF" +
            "KsIQB6wAAAAAAAA=";
    // W22J: version 49, jsr / ret.
    static final String J_B64 = "yv66vgAAADEAEgEABFcyMkoHAAEBABBqYXZhL2xhbmcvT2JqZWN0BwADAQAFdHdpY2UBAAQoSSlJAQAHd2lkZVJl" +
            "dAEACmpzckZpbmFsbHkBABpqYXZhL2xhbmcvUnVudGltZUV4Y2VwdGlvbgcACQEAA25lZwgACwEABjxpbml0PgEA" +
            "FShMamF2YS9sYW5nL1N0cmluZzspVgwADQAOCgAKAA8BAARDb2RlACEAAgAEAAAAAAADAAkABQAGAAEAEQAAABwA" +
            "AgADAAAAEBo8qAAIqAAFG6xNhAEKqQIAAAAAAAkABwAGAAEAEQAAAB4AAgEtAAAAEho8qAAFG6zEOgEshAH7xKkB" +
            "LAAAAAAACQAIAAYAAQARAAAAOQAGAAQAAAAlAzwanAANuwAKWRIMtwAQvxoFaDyoAAsbrE6oAAUtv02EAWSpAgAB" +
            "AAIAFAAZAAAAAAAA";
    // W22S: version 69, switch / wide / exception-table shapes.
    static final String S_B64 = "yv66vgAAAEUALQEABFcyMlMHAAEBABBqYXZhL2xhbmcvT2JqZWN0BwADAQAGdHNNaW4wAQAEKEkpSQEABnRzTWF4" +
            "MAEAA2xzMAEABnRzTWluMQEABnRzTWF4MQEAA2xzMQEABnRzTWluMgEABnRzTWF4MgEAA2xzMgEABnRzTWluMwEA" +
            "BnRzTWF4MwEAA2xzMwEABXdpZGVJAQAFd2lkZUoBAAQoSilKAQAFd2lkZUYBAAQoRilGAQAFd2lkZUQBAAQoRClE" +
            "AQAFd2lkZUEBACYoTGphdmEvbGFuZy9PYmplY3Q7KUxqYXZhL2xhbmcvT2JqZWN0OwEACm5hcnJvd0lpbmMBAAxl" +
            "bmRFeGNsdXNpdmUBAAMoKUkBAA5zdGFydEluY2x1c2l2ZQEAC3NlbGZIYW5kbGVyAQAaamF2YS9sYW5nL1J1bnRp" +
            "bWVFeGNlcHRpb24HACABAAY8aW5pdD4BAAMoKVYMACIAIwoAIQAkAQAKZmlyc3RNYXRjaAEAD2phdmEvbGFuZy9F" +
            "cnJvcgcAJwEAE2phdmEvbGFuZy9UaHJvd2FibGUHACkBAARDb2RlAQANU3RhY2tNYXBUYWJsZQAhAAIABAAAAAAA" +
            "FgAJAAUABgABACsAAAA8AAEAAQAAACQaqgAAAAAAIYAAAACAAAACAAAAGwAAAB0AAAAfBKwFrAasAqwAAAABACwA" +
            "AAAGAAQcAQEBAAkABwAGAAEAKwAAADwAAQABAAAAJBqqAAAAAAAhf////X////8AAAAbAAAAHQAAAB8ErAWsBqwC" +
            "rAAAAAEALAAAAAYABBwBAQEACQAIAAYAAQArAAAATwABAAEAAAA2GqsAAAAAADMAAAAEgAAAAAAAACv/////AAAA" +
            "LQAAAAAAAAAvf////wAAADEErAWsBqwHrAKsAAAAAQAsAAAABwAFLAEBAQEACQAJAAYAAQArAAAAPAABAAEAAAAk" +
            "ABqqAAAAACCAAAAAgAAAAgAAABoAAAAcAAAAHgSsBawGrAKsAAAAAQAsAAAABgAEHAEBAQAJAAoABgABACsAAAA8" +
            "AAEAAQAAACQAGqoAAAAAIH////1/////AAAAGgAAABwAAAAeBKwFrAasAqwAAAABACwAAAAGAAQcAQEBAAkACwAG" +
            "AAEAKwAAAE8AAQABAAAANgAaqwAAAAAyAAAABIAAAAAAAAAq/////wAAACwAAAAAAAAALn////8AAAAwBKwFrAas" +
            "B6wCrAAAAAEALAAAAAcABSwBAQEBAAkADAAGAAEAKwAAADwAAQABAAAAJAAAGqoAAAAfgAAAAIAAAAIAAAAZAAAA" +
            "GwAAAB0ErAWsBqwCrAAAAAEALAAAAAYABBwBAQEACQANAAYAAQArAAAAPAABAAEAAAAkAAAaqgAAAB9////9f///" +
            "/wAAABkAAAAbAAAAHQSsBawGrAKsAAAAAQAsAAAABgAEHAEBAQAJAA4ABgABACsAAABPAAEAAQAAADYAABqrAAAA" +
            "MQAAAASAAAAAAAAAKf////8AAAArAAAAAAAAAC1/////AAAALwSsBawGrAesAqwAAAABACwAAAAHAAUsAQEBAQAJ" +
            "AA8ABgABACsAAABAAAEAAQAAACgAAAAaqgAAAAAAACKAAAAAgAAAAgAAABwAAAAeAAAAIASsBawGrAKsAAAAAQAs" +
            "AAAABgAEIAEBAQAJABAABgABACsAAABAAAEAAQAAACgAAAAaqgAAAAAAACJ////9f////wAAABwAAAAeAAAAIASs" +
            "BawGrAKsAAAAAQAsAAAABgAEIAEBAQAJABEABgABACsAAABTAAEAAQAAADoAAAAaqwAAAAAAADQAAAAEgAAAAAAA" +
            "ACz/////AAAALgAAAAAAAAAwf////wAAADIErAWsBqwHrAKsAAAAAQAsAAAABwAFMAEBAQEACQASAAYAAQArAAAA" +
            "IgABAS0AAAAWGsQ2ASzEhAEsitDEhAEsf//EFQEsrAAAAAAACQATABQAAQArAAAAIAAEAZIAAAAUHsQ3AS3EFgEt" +
            "CmXENwGQxBYBkK0AAAAAAAkAFQAWAAEAKwAAABcAAQEsAAAACyLEOAErxBcBK3auAAAAAAAJABcAGAABACsAAAAX" +
            "AAIB9gAAAAsmxDkB9MQYAfR3rwAAAAAACQAZABoAAQArAAAAGwABAQIAAAAPKsQ6AQABxDoBAcQZAQCwAAAAAAAJ" +
            "ABsABgABACsAAAAXAAEAAQAAAAuEAICEAH+EAP8arAAAAAAACQAcAB0AAQArAAAAKAABAAAAAAAIBFcBv1cQKqwA" +
            "AQAAAAIABAAAAAEALAAAAAYAAUQHACoACQAeAB0AAQArAAAAJgABAAAAAAAGAb9XECusAAEAAQACAAIAAAABACwA" +
            "AAAGAAFCBwAqAAkAHwAdAAEAKwAAAD4AAgABAAAAFgM7AVeEAAEaBqIAC7sAIVm3ACW/GqwAAQADABQAAwAAAAEA" +
            "LAAAAA4AAv8AAwABAQABBwAqEAAJACYAHQABACsAAABJAAIAAAAAABG7ACFZtwAlv1cErFcFrFcGrAADAAAACAAI" +
            "ACgAAAAIAAsAIQAAAAgADgAqAAEALAAAAA4AA0gHAChCBwAhQgcAKgAA";

    static final class Loader extends ClassLoader {
        Loader() {
            super(L7W22HandBytecode.class.getClassLoader());
        }

        Class<?> define(String name, String b64) {
            byte[] b = Base64.getDecoder().decode(b64);
            return defineClass(name, b, 0, b.length);
        }
    }

    static Object call(Class<?> c, String n, Class<?>[] p, Object... a) throws Throwable {
        Method m = c.getMethod(n, p);
        try {
            return m.invoke(null, a);
        } catch (InvocationTargetException e) {
            throw e.getCause();
        }
    }

    static String t(Throwable e) {
        return e.getClass().getName() + ": " + e.getMessage();
    }

    public static void main(String[] args) throws Throwable {
        Loader l = new Loader();
        Class<?> m = l.define("W22M", M_B64);
        Class<?> j = l.define("W22J", J_B64);
        Class<?> s = l.define("W22S", S_B64);
        Class<?>[] obj = {Object.class};
        Class<?>[] i1 = {int.class};

        String[] monitorShapes = {"enterOnly", "exitOnly", "enterTwiceExitOnce", "enterThenThrow",
                "enterReturnInt", "callerThenCalleeExits"};
        for (String n : monitorShapes) {
            Object lock = new Object();
            try {
                System.out.println("mon " + n + " -> " + call(m, n, obj, lock));
            } catch (Throwable e) {
                System.out.println("mon " + n + " threw " + t(e));
            }
            System.out.println("mon " + n + " holdsLock " + Thread.holdsLock(lock));
        }
        System.out.println("mon reached " + m.getField("reached").getInt(null));

        System.out.println("jsr twice " + call(j, "twice", i1, 5) + " wideRet " + call(j, "wideRet", i1, 5)
                + " finally " + call(j, "jsrFinally", i1, 4));
        try {
            call(j, "jsrFinally", i1, -1);
            System.out.println("jsr finally returned");
        } catch (Throwable e) {
            System.out.println("jsr finally threw " + t(e));
        }

        int[] keys = {Integer.MIN_VALUE, Integer.MIN_VALUE + 1, Integer.MIN_VALUE + 2, Integer.MIN_VALUE + 3,
                -1, 0, 1, Integer.MAX_VALUE - 3, Integer.MAX_VALUE - 2, Integer.MAX_VALUE - 1, Integer.MAX_VALUE};
        for (int p = 0; p < 4; p++) {
            StringBuilder sb = new StringBuilder("switch pad" + p);
            for (String n : new String[] {"tsMin", "tsMax", "ls"}) {
                sb.append(' ').append(n).append(':');
                for (int k : keys) {
                    sb.append((int) (Integer) call(s, n + p, i1, k)).append(',');
                }
            }
            System.out.println(sb);
        }

        System.out.println("wide I " + call(s, "wideI", i1, 5)
                + " J " + call(s, "wideJ", new Class<?>[] {long.class}, 0L)
                + " F " + call(s, "wideF", new Class<?>[] {float.class}, 1.5f)
                + " D " + call(s, "wideD", new Class<?>[] {double.class}, 0.0)
                + " A " + call(s, "wideA", obj, "s")
                + " iinc " + call(s, "narrowIinc", i1, 10));

        for (String n : new String[] {"endExclusive", "startInclusive", "selfHandler", "firstMatch"}) {
            try {
                System.out.println("exc " + n + " -> " + call(s, n, new Class<?>[0]));
            } catch (Throwable e) {
                // Class only: the helpful-NPE text of a generated class
                // without a LocalVariableTable is not the point here.
                System.out.println("exc " + n + " threw " + e.getClass().getName());
            }
        }
    }
}
