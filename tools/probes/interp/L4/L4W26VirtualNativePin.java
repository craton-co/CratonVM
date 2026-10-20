// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 26, lane L4: a virtual thread that parks or
// sleeps UNDER a Rust frame with post-call work -- a native that re-entered
// Java (`Method.invoke`, `MethodHandle.invoke` / `invokeExact` /
// `invokeWithArguments`), a constructor reference's `<init>`, a dynamic
// proxy's handler, a `<clinit>` -- gets that frame's answer. Page:
// docs/internal/fixed-bugs/interpreter-L4-a-virtual-thread-that-unmounts-under-a-native-loses-the-natives-return-conversion-FIXED-20260928.md
//
// CratonVM's continuation is the thread's Java frames only; a yield unwound
// the Rust frames between the parking native and the carrier, so the
// remounted callee returned straight into the Java caller: a `void` method
// reached through `Method.invoke` answered no value where `invokeImpl`'s
// `areturn` expected an `Object` (operand-stack underflow, the virtual thread
// died), an `int` one handed the caller a raw `int` where a boxed `Integer` was
// due, `Foo::new` answered nothing, a `<clinit>` that parked left its class
// marked failed. Since wave 26 such a continuation is PINNED, as HotSpot pins
// one with a native frame on it: the park blocks the carrier.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W26VirtualNativePin
//
// HotSpot 25 (25.0.3), plain and with
// `-XX:+UnlockExperimentalVMOptions -XX:-VMContinuations` (as the L7
// virtual-thread probes compare), prints exactly:
//     Method.invoke void sleep: null
//     Method.invoke int sleep: 7 java.lang.Integer
//     Method.invoke int park: 5 java.lang.Integer
//     invokeExact int sleep: 7
//     invokeExact void sleep: done
//     invoke (Object) int sleep: 7 java.lang.Integer
//     invokeWithArguments void sleep: null
//     constructor reference sleep: 3
//     proxy handler sleep: 9
//     clinit sleep: 42
//     Method.invoke String sleep: bean
//     Method.invoke static String sleep: static-x
//     invokeExact String sleep: bean
//     insertArguments invoke String sleep: static-bound
//
// The last four rows return a reference the method-handle door hands back
// unchanged: there the door is TRANSPARENT (`MH_TAIL` in `lang_invoke.rs`,
// `cratonvm_native_api::continuation_pin::transparent`) and the virtual
// thread unmounts beneath it, as on HotSpot; they already printed these lines
// before wave 26 and must keep doing so.
//
// CratonVM before (read from the code, not run): the first row's virtual thread
// died with an internal "operand-stack underflow at value return", rows 2, 3
// and 6 answered a raw `int` into an `Object` slot, row 8 answered nothing, and
// the `<clinit>` row a `NoClassDefFoundError` (the yield reached the class
// initializer as a failure).
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.util.concurrent.atomic.AtomicReference;
import java.util.concurrent.locks.LockSupport;
import java.util.function.IntSupplier;
import java.util.function.Supplier;

public class L4W26VirtualNativePin {
    interface Body {
        Object run() throws Throwable;
    }

    public static class Bean {
        public void work() throws InterruptedException {
            Thread.sleep(10);
        }

        public int count() throws InterruptedException {
            Thread.sleep(10);
            return 7;
        }

        public String name() throws InterruptedException {
            Thread.sleep(10);
            return "bean";
        }

        public static String staticName(String suffix) throws InterruptedException {
            Thread.sleep(10);
            return "static-" + suffix;
        }

        static volatile Thread parked;
        static volatile boolean released;

        public int parkUntilReleased() {
            parked = Thread.currentThread();
            while (!released) {
                LockSupport.park(this);
            }
            return 5;
        }
    }

    static final class Slow {
        final int v;

        Slow() {
            try {
                Thread.sleep(10);
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
            v = 3;
        }
    }

    static final class Init {
        static final int V;

        static {
            try {
                Thread.sleep(10);
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
            V = 42;
        }
    }

    static String describe(Object r) {
        return r == null ? "null" : r + " " + r.getClass().getName();
    }

    static void row(String label, Body body) throws InterruptedException {
        AtomicReference<String> out = new AtomicReference<>("(no answer)");
        Thread t = Thread.ofVirtual().start(() -> {
            try {
                out.set(String.valueOf(body.run()));
            } catch (Throwable e) {
                out.set(e.toString());
            }
        });
        t.join(20_000);
        System.out.println(label + ": " + (t.isAlive() ? "(still running)" : out.get()));
    }

    public static void main(String[] args) throws Throwable {
        Bean bean = new Bean();
        Method work = Bean.class.getMethod("work");
        Method count = Bean.class.getMethod("count");
        Method park = Bean.class.getMethod("parkUntilReleased");
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandle countH = lookup.findVirtual(Bean.class, "count", MethodType.methodType(int.class));
        MethodHandle workH = lookup.findVirtual(Bean.class, "work", MethodType.methodType(void.class));

        row("Method.invoke void sleep", () -> String.valueOf(work.invoke(bean)));
        row("Method.invoke int sleep", () -> describe(count.invoke(bean)));

        // A platform thread releases the virtual thread once it is parked (or
        // after 2 s of not seeing it parked), with ONE unpark: a pinned
        // virtual thread parks on its carrier, and the unpark must reach it
        // there. A lost wakeup prints `(still running)`.
        Thread releaser = new Thread(() -> {
            try {
                while (Bean.parked == null) {
                    Thread.sleep(1);
                }
                long give_up = System.nanoTime() + 2_000_000_000L;
                while (Bean.parked.getState() != Thread.State.WAITING && System.nanoTime() < give_up) {
                    Thread.sleep(1);
                }
            } catch (InterruptedException e) {
                return;
            }
            Bean.released = true;
            LockSupport.unpark(Bean.parked);
        });
        releaser.setDaemon(true);
        releaser.start();
        row("Method.invoke int park", () -> describe(park.invoke(bean)));

        row("invokeExact int sleep", () -> (int) countH.invokeExact(bean));
        row("invokeExact void sleep", () -> {
            workH.invokeExact(bean);
            return "done";
        });
        row("invoke (Object) int sleep", () -> describe((Object) countH.invoke(bean)));
        row("invokeWithArguments void sleep", () -> String.valueOf(workH.invokeWithArguments(bean)));

        Supplier<Slow> ctor = Slow::new;
        row("constructor reference sleep", () -> ctor.get().v);

        IntSupplier proxy = (IntSupplier) Proxy.newProxyInstance(
                L4W26VirtualNativePin.class.getClassLoader(),
                new Class<?>[] {IntSupplier.class},
                (p, m, a) -> {
                    Thread.sleep(10);
                    return 9;
                });
        row("proxy handler sleep", () -> proxy.getAsInt());

        row("clinit sleep", () -> Init.V);

        // A reference result handed back unchanged: the method-handle door is
        // transparent, so the virtual thread may unmount beneath it, as on
        // HotSpot (no pin); the answer must be the same.
        Method name = Bean.class.getMethod("name");
        Method staticName = Bean.class.getMethod("staticName", String.class);
        MethodHandle nameH = lookup.findVirtual(Bean.class, "name", MethodType.methodType(String.class));
        MethodHandle staticNameH = MethodHandles.insertArguments(lookup.findStatic(Bean.class, "staticName",
                MethodType.methodType(String.class, String.class)), 0, "bound");
        row("Method.invoke String sleep", () -> name.invoke(bean));
        row("Method.invoke static String sleep", () -> staticName.invoke(null, "x"));
        row("invokeExact String sleep", () -> (String) nameH.invokeExact(bean));
        row("insertArguments invoke String sleep", () -> (String) staticNameH.invoke());
    }
}
