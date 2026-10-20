// Interpreter round i1 wave 52, lane L1: the functions of the C JVMTI table
// that most native agents call first, against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-lacks-the-functions-and-events-agents-call-first-FIXED-20261007.md):
// GetPhase, Get/SetEnvironmentLocalStorage, the raw monitors,
// GetThreadInfo, GetTopThreadGroups, GetThreadGroupInfo, the class facts
// (GetClassModifiers, IsInterface, IsArrayClass, GetSourceFileName,
// GetClassLoader, GetImplementedInterfaces, GetClassVersionNumbers), the
// field facts (GetClassFields, GetFieldName, GetFieldModifiers,
// GetFieldDeclaringClass, IsFieldSynthetic), Get/SetThreadLocalStorage,
// GetBytecodes, GetMaxLocals / GetArgumentsSize / IsMethodSynthetic, and
// the ThreadStart / ThreadEnd events.
//
// The shim's `Agent_OnLoad` creates a raw monitor, stores a pointer in its
// env's local storage and enables ThreadStart / ThreadEnd (what agents do
// first, in OnLoad); `threadEvents` lists those of the probe's own threads
// after the worker ended. The
// native `rows` prints one line per fact; the first row (GetThreadInfo of
// the calling thread) is printed again at the end, so a first-call-only gap
// shows as a differing row.
//
// Needs the native shim tools/probes/interp/L1/L1W52JvmtiThreadAndClassFacts.c,
// loaded both as an agent and with `System.load` (for `rows`):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w52facts.so tools/probes/interp/L1/L1W52JvmtiThreadAndClassFacts.c
//   javac -g -d /tmp/l1w52facts tools/probes/interp/L1/L1W52JvmtiThreadAndClassFacts.java
//   java     -agentpath:/tmp/libl1w52facts.so -cp /tmp/l1w52facts L1W52JvmtiThreadAndClassFacts /tmp/libl1w52facts.so
//   cratonvm -agentpath:/tmp/libl1w52facts.so -cp /tmp/l1w52facts L1W52JvmtiThreadAndClassFacts /tmp/libl1w52facts.so
//
// Without the argument it prints only the usage line.
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with a Rust
// port of the C shim (the box has no C compiler; same calls, same order,
// same output format), three runs, the same each time. The orchestrator
// should take the host's HotSpot 25.0.4 run of the C shim as the reference.
//
//   onload: caps err=0 monitor err=0 storage err=0 events err=0
//   thread NULL: err=0 name=main prio=5 daemon=0 group=main loader=app
//   phase: err=0 phase=4
//   storage: err=0 same=1
//   raw monitor: enter=0 enter=0 exit=0 wait=0 notify=0 notifyAll=0 exit=0 exit=51 destroy=0
//   thread storage: set=0 get=0 same=1 worker=0 null=1
//   thread worker: err=0 name=facts-worker prio=7 daemon=1 group=facts-group loader=app
//   thread unstarted: err=0 name=facts-unstarted prio=5 daemon=0 group=main loader=app
//   group facts-group: err=0 name=facts-group parent=main max=8 daemon=0
//   group main: err=0 name=main parent=system max=10 daemon=0
//   top groups: err=0 count=1 first=system parent=null
//   class LL1W52JvmtiThreadAndClassFacts;: mods=0x21 interface=0 array=0 source=0:L1W52JvmtiThreadAndClassFacts.java loader=app interfaces=0 methods=0 version=0:69
//   class Ljava/lang/String;: mods=0x31 interface=0 array=0 source=0:String.java loader=null interfaces=5 Ljava/io/Serializable; Ljava/lang/Comparable; Ljava/lang/CharSequence; Ljava/lang/constant/Constable; Ljava/lang/constant/ConstantDesc; methods=0 version=0:69
//   class LL1W52JvmtiThreadAndClassFacts$Pair;: mods=0x38 interface=0 array=0 source=0:L1W52JvmtiThreadAndClassFacts.java loader=app interfaces=0 methods=0 version=0:69
//   class LL1W52JvmtiThreadAndClassFacts$Shape;: mods=0x628 interface=1 array=0 source=0:L1W52JvmtiThreadAndClassFacts.java loader=app interfaces=0 methods=0 version=0:69
//   class LL1W52JvmtiThreadAndClassFacts$Base;: mods=0x428 interface=0 array=0 source=0:L1W52JvmtiThreadAndClassFacts.java loader=app interfaces=2 LL1W52JvmtiThreadAndClassFacts$Shape; Ljava/lang/Runnable; methods=0 version=0:69
//   class [I: mods=0x431 interface=0 array=1 source=101 loader=null interfaces=0 methods=0 version=101
//   class [Ljava/lang/String;: mods=0x431 interface=0 array=1 source=101 loader=null interfaces=0 methods=0 version=101
//   class Ljava/lang/Runnable;: mods=0x621 interface=1 array=0 source=0:Runnable.java loader=null interfaces=0 methods=0 version=0:69
//   class LL1W52JvmtiThreadAndClassFacts$Unlinked;: mods=0x38 interface=0 array=0 source=0:L1W52JvmtiThreadAndClassFacts.java loader=app interfaces=err=22 methods=22 version=0:69
//   fields of Pair: err=0 count=5
//   field LABEL Ljava/lang/String; generic=- mods=0x19 declaring=LL1W52JvmtiThreadAndClassFacts$Pair; synthetic=0
//   field count I generic=- mods=0x2 declaring=LL1W52JvmtiThreadAndClassFacts$Pair; synthetic=0
//   field stamp J generic=- mods=0x44 declaring=LL1W52JvmtiThreadAndClassFacts$Pair; synthetic=0
//   field names Ljava/util/List; generic=Ljava/util/List<Ljava/lang/String;>; mods=0x80 declaring=LL1W52JvmtiThreadAndClassFacts$Pair; synthetic=0
//   field created I generic=- mods=0x8 declaring=LL1W52JvmtiThreadAndClassFacts$Pair; synthetic=0
//   jni ids: static created=42 name=created instance count name=count
//   fields of Inner: err=0 count=1
//   field this$0 LL1W52JvmtiThreadAndClassFacts; generic=- mods=0x1010 declaring=LL1W52JvmtiThreadAndClassFacts$Inner; synthetic=1
//   bytecodes sum: err=0 count=20 1a851f61370416042dc7000703a70004
//   method sum: maxLocals=0:6 argsSize=0:4 synthetic=0:0
//   bytecodes run: err=0 count=1 b1
//   method run: maxLocals=0:1 argsSize=0:1 synthetic=0:0
//   thread NULL again: err=0 name=main prio=5 daemon=0 group=main loader=app
//   thread events: start facts-worker end facts-worker
//
// Two answers differ from what reading the specification suggests, and the
// table follows HotSpot: an interface's modifiers carry ACC_SUPER (0x621 for
// Runnable, 0x628 for the nested `static interface`), and GetClassFields /
// GetImplementedInterfaces / GetClassMethods answer
// JVMTI_ERROR_CLASS_NOT_PREPARED (22) for a
// class that is loaded but not linked yet (`Unlinked`; before `main` linked
// them, `Pair`, `Base`, `Shape` and `Inner` answered 22 too).
//
// CratonVM before wave 52 (from reading jvmti::native_env): every function
// above but GetClassSignature answered JVMTI_ERROR_NOT_AVAILABLE (98), the
// raw monitor in OnLoad included, and AddCapabilities of
// can_get_source_file_name, can_get_synthetic_attribute and
// can_get_bytecodes answered 98 (`onload: caps err=98 monitor
// err=98 storage err=98 events err=98`; ThreadStart and ThreadEnd
// were not delivered: `thread events:`).
//
// CratonVM since wave 52, predicted from the code: HotSpot's lines. The
// rows most likely to differ: `Unlinked` (CratonVM must not link a class its
// class literal names; `jvmti::native_env::prepared` reads the class state)
// and the `jni ids` row (a `GetStaticFieldID` id carries the static mark,
// `jni::STATIC_FIELD_ID_MARK`, so GetFieldName names `created`, not the
// instance field of the same slot number). No debug line: each row is its
// own positive control. Unit tests:
// `jvmti::native_env::tests::the_wave_52_facts_answer_through_the_c_table`.
import java.util.List;
import java.util.concurrent.CountDownLatch;

public class L1W52JvmtiThreadAndClassFacts {
    static native String threadEvents();

    static native String rows(Thread worker, Thread unstarted, ClassLoader app, Class<?>[] classes,
            Class<?> pair, Class<?> inner);

    interface Shape {
    }

    abstract static class Base implements Shape, Runnable {
    }

    static final class Pair extends Base {
        public static final String LABEL = "pair";
        private int count;
        protected volatile long stamp;
        transient List<String> names;
        static int created = 42;

        public void run() {
        }

        static long sum(int a, long b, Object c) {
            long s = a + b;
            return s + (c == null ? 0 : 1);
        }
    }

    /** Named only by its class literal: loaded, never linked. */
    static final class Unlinked {
        int unseen;
    }

    class Inner {
        // javac (JDK 18+) writes the `this$0` field only for an inner class
        // that uses its outer instance.
        Object outer() {
            return L1W52JvmtiThreadAndClassFacts.this;
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W52JvmtiThreadAndClassFacts <path of the shim library>");
            return;
        }
        System.load(args[0]);
        ThreadGroup group = new ThreadGroup("facts-group");
        group.setMaxPriority(8);
        CountDownLatch started = new CountDownLatch(1);
        CountDownLatch release = new CountDownLatch(1);
        Thread worker = new Thread(group, () -> {
            started.countDown();
            try {
                release.await();
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        }, "facts-worker");
        worker.setDaemon(true);
        worker.setPriority(7);
        worker.start();
        started.await();
        Thread unstarted = new Thread(() -> { }, "facts-unstarted");
        // Link (and prepare) the classes whose fields and interfaces are
        // listed: GetClassFields and GetImplementedInterfaces answer
        // JVMTI_ERROR_CLASS_NOT_PREPARED before (Unlinked shows it).
        new Pair().run();
        new L1W52JvmtiThreadAndClassFacts().new Inner().outer();
        Class<?>[] classes = {
            L1W52JvmtiThreadAndClassFacts.class, String.class, Pair.class, Shape.class, Base.class,
            int[].class, String[].class, Runnable.class, Unlinked.class,
        };
        System.out.print(rows(worker, unstarted, ClassLoader.getSystemClassLoader(), classes,
                Pair.class, Inner.class));
        release.countDown();
        worker.join();
        System.out.print(threadEvents());
    }
}
