// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 115, lane L4 (`CRATONVM_MH_VOID_TAIL_UNMOUNT`,
// default off; proposal
// docs/internal/fixed-bugs/interpreter-i26-L4-proposal-let-a-virtual-thread-unmount-under-the-method-handle-natives-KEPT-OFF-20261224.md,
// the `V -> reference` half): a virtual thread that parks inside a `void`
// method reached through a method-handle door whose caller receives a
// REFERENCE (the door's `null`: `invoke` at an `Object` site, `asType` to
// `()Object`, `invokeWithArguments`, `Method.invoke` under `--jdk-only`) now
// unmounts beneath the door, as on HotSpot, and the remounted frame's `void`
// return pushes that `null`. Rows 6, 8 and 9 reach a leaf that may be a VM
// native which yields by itself (no leaf frame): the `null` then goes onto
// the caller's stack at the yield. Rows 7-9 keep live values on the caller's
// operand stack under the call, so a lost or doubled `null` shows as a wrong
// row or a crash, not as a quiet `null`.
//
// Every row must print HotSpot's line with the switch off (they pin, as
// before) and on (they unmount), both modes, JIT and --nojit.
//
// Positive control (switch on): `CRATONVM_DBG_MH_DISPATCH=1` prints one
// `[MH_TAIL_ADAPTER] ... caller_return=Ljava/lang/Object; void-leaf` line per
// park that unmounted under a door: expect rows 1-3 and 5-9 in both modes (row
// 4 under `--jdk-only` only, where `Method.invoke` is a method handle), plus
// up to 64 from row 11. A row whose door is not a served leaf lane (a JDK
// `LambdaForm` runs the leaf instead) records none and is still right. With
// the switch off there is no `void-leaf` line at all.
//
// Run: javac -d . L4W115VirtualVoidLeafUnmount.java
//      java -cp . L4W115VirtualVoidLeafUnmount                                   (HotSpot)
//      cratonvm --jdk-only -cp . L4W115VirtualVoidLeafUnmount
//      CRATONVM_MH_VOID_TAIL_UNMOUNT=1 cratonvm --jdk-only -cp . L4W115VirtualVoidLeafUnmount
//      CRATONVM_MH_VOID_TAIL_UNMOUNT=1 CRATONVM_DBG_MH_DISPATCH=1 cratonvm --jdk-only -cp . \
//          L4W115VirtualVoidLeafUnmount 2>&1 | grep -c 'void-leaf'
//      (and each with --compatible and with --nojit)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// 1 invoke Object site void: null
// 2 invokeWithArguments void: null
// 3 asType Object invokeExact void: null
// 4 Method.invoke void: null
// 5 insertArguments invoke Object void: null
// 6 invoke Object site Thread.sleep: null
// 7 live operands under a Java void leaf: 1 2 null 3
// 8 live operands under Thread.sleep: 1 2 null 3
// 9 live operands under CountDownLatch.await: 1 2 null 3
// 10 after the rows: work=5
// 11 many virtual threads: 64
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;

public class L4W115VirtualVoidLeafUnmount {
    interface Body {
        Object run() throws Throwable;
    }

    static final AtomicInteger WORK = new AtomicInteger();

    public static void work() throws InterruptedException {
        Thread.sleep(10);
        WORK.incrementAndGet();
    }

    public static void workWith(int n) throws InterruptedException {
        Thread.sleep(n);
    }

    static String three(int a, int b, Object c, int d) {
        return a + " " + b + " " + c + " " + d;
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
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> me = L4W115VirtualVoidLeafUnmount.class;
        MethodHandle workH = l.findStatic(me, "work", MethodType.methodType(void.class));
        MethodHandle workObj = workH.asType(MethodType.methodType(Object.class));
        MethodHandle withH = l.findStatic(me, "workWith", MethodType.methodType(void.class, int.class));
        MethodHandle with10 = MethodHandles.insertArguments(withH, 0, 10);
        MethodHandle sleepH = l.findStatic(Thread.class, "sleep", MethodType.methodType(void.class, long.class));
        MethodHandle sleepObj = sleepH.asType(MethodType.methodType(Object.class, long.class));
        MethodHandle awaitH = l.findVirtual(CountDownLatch.class, "await", MethodType.methodType(void.class));
        MethodHandle awaitObj = awaitH.asType(MethodType.methodType(Object.class, CountDownLatch.class));
        Method workM = me.getMethod("work");

        row("1 invoke Object site void", () -> (Object) workH.invoke());
        row("2 invokeWithArguments void", () -> workH.invokeWithArguments());
        row("3 asType Object invokeExact void", () -> (Object) workObj.invokeExact());
        row("4 Method.invoke void", () -> workM.invoke(null));
        row("5 insertArguments invoke Object void", () -> (Object) with10.invoke());
        row("6 invoke Object site Thread.sleep", () -> (Object) sleepH.invoke(10L));
        row("7 live operands under a Java void leaf",
                () -> three(1, 2, (Object) workObj.invokeExact(), 3));
        row("8 live operands under Thread.sleep",
                () -> three(1, 2, (Object) sleepObj.invokeExact(10L), 3));
        row("9 live operands under CountDownLatch.await", () -> {
            CountDownLatch latch = new CountDownLatch(1);
            Thread.ofVirtual().start(() -> {
                try {
                    Thread.sleep(20);
                } catch (InterruptedException e) {
                    // not interrupted
                }
                latch.countDown();
            });
            return three(1, 2, (Object) awaitObj.invokeExact(latch), 3);
        });
        System.out.println("10 after the rows: work=" + WORK.get());

        // Many virtual threads parked under the door at once: the values must
        // all arrive; the wall time (a pinned door holds one carrier each) is
        // a TIME line only.
        int n = 64;
        AtomicInteger nulls = new AtomicInteger();
        List<Thread> threads = new ArrayList<>();
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            threads.add(Thread.ofVirtual().start(() -> {
                try {
                    Object r = workObj.invokeExact();
                    if (r == null) {
                        nulls.incrementAndGet();
                    }
                } catch (Throwable e) {
                    System.out.println("many: " + e);
                }
            }));
        }
        for (Thread t : threads) {
            t.join(60_000);
        }
        long ms = (System.nanoTime() - t0) / 1_000_000;
        System.out.println("11 many virtual threads: " + nulls.get());
        System.out.println("TIME many-virtual-threads-ms " + ms);
    }
}
