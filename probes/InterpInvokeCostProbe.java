// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Marginal cost of one method INVOCATION, by kind, against an `iadd` control.
//
// The 2026-08-18 interpreter audit fixed five instances of one shape -- the
// interpreter re-deriving per execution an answer fixed per call site -- all of
// them in single opcodes: back-edge counters, field sites, eleven armless
// opcodes, cast sites, ldc constants. Single opcodes are not where real Java
// throughput lives; invocation is. Nothing in that audit measured it.
//
// Each kernel runs UNROLL calls per loop iteration and is differenced against
// an identical loop with UNROLL fewer, so what is reported is the marginal cost
// of ONE invocation. Every callee is trivial (return 1) so the number is
// dispatch plus frame setup and teardown, not callee body.
//
// Read the kinds against each other, not just against iadd:
//   * static vs virtual isolates receiver dispatch.
//   * virtual-mono vs iface-4receiver isolates the inline cache. Four receiver
//     classes is past the point a PIC in this tree thrashes.
//   * interface vs virtual isolates itable/name lookup.
//   * final/private isolates whether "cannot be overridden" is exploited.
//   * static-native is the path the MethodSiteCache actually sits on --
//     pop_coerced_invoke_args_* -- so it is the arm that cache could move.
public final class InterpInvokeCostProbe {

    static final int UNROLL = 16;

    interface Iface { int get(); }
    static class A implements Iface { public int get() { return 1; } }
    static class B implements Iface { public int get() { return 2; } }
    static class C implements Iface { public int get() { return 3; } }
    static class D implements Iface { public int get() { return 4; } }

    static class Base { int virt() { return 1; } }
    static class Sub extends Base { @Override int virt() { return 2; } }

    static int stat() { return 1; }
    final int fin() { return 1; }
    int inst() { return 1; }

    // --- invokestatic ---
    static int staticKernel(int iters) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += stat(); acc += stat(); acc += stat(); acc += stat();
            acc += stat(); acc += stat(); acc += stat(); acc += stat();
            acc += stat(); acc += stat(); acc += stat(); acc += stat();
            acc += stat(); acc += stat(); acc += stat(); acc += stat();
        }
        return acc;
    }
    static int staticBase(int iters) {
        int acc = 0;
        for (int i = 0; i < iters; i++) { acc += stat(); }
        return acc;
    }

    // --- invokevirtual, monomorphic ---
    static int virtMonoKernel(int iters, Base b) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += b.virt(); acc += b.virt(); acc += b.virt(); acc += b.virt();
            acc += b.virt(); acc += b.virt(); acc += b.virt(); acc += b.virt();
            acc += b.virt(); acc += b.virt(); acc += b.virt(); acc += b.virt();
            acc += b.virt(); acc += b.virt(); acc += b.virt(); acc += b.virt();
        }
        return acc;
    }
    static int virtMonoBase(int iters, Base b) {
        int acc = 0;
        for (int i = 0; i < iters; i++) { acc += b.virt(); }
        return acc;
    }

    // --- invokeinterface, monomorphic ---
    static int ifaceMonoKernel(int iters, Iface o) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += o.get(); acc += o.get(); acc += o.get(); acc += o.get();
            acc += o.get(); acc += o.get(); acc += o.get(); acc += o.get();
            acc += o.get(); acc += o.get(); acc += o.get(); acc += o.get();
            acc += o.get(); acc += o.get(); acc += o.get(); acc += o.get();
        }
        return acc;
    }
    static int ifaceMonoBase(int iters, Iface o) {
        int acc = 0;
        for (int i = 0; i < iters; i++) { acc += o.get(); }
        return acc;
    }

    // --- invokeinterface, 4 receivers: past the PIC capacity ---
    static int ifacePolyKernel(int iters, Iface[] os) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += os[0].get(); acc += os[1].get(); acc += os[2].get(); acc += os[3].get();
            acc += os[0].get(); acc += os[1].get(); acc += os[2].get(); acc += os[3].get();
            acc += os[0].get(); acc += os[1].get(); acc += os[2].get(); acc += os[3].get();
            acc += os[0].get(); acc += os[1].get(); acc += os[2].get(); acc += os[3].get();
        }
        return acc;
    }
    static int ifacePolyBase(int iters, Iface[] os) {
        int acc = 0;
        for (int i = 0; i < iters; i++) { acc += os[0].get(); }
        return acc;
    }

    // --- final instance call ---
    static int finalKernel(int iters, InterpInvokeCostProbe p) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += p.fin(); acc += p.fin(); acc += p.fin(); acc += p.fin();
            acc += p.fin(); acc += p.fin(); acc += p.fin(); acc += p.fin();
            acc += p.fin(); acc += p.fin(); acc += p.fin(); acc += p.fin();
            acc += p.fin(); acc += p.fin(); acc += p.fin(); acc += p.fin();
        }
        return acc;
    }
    static int finalBase(int iters, InterpInvokeCostProbe p) {
        int acc = 0;
        for (int i = 0; i < iters; i++) { acc += p.fin(); }
        return acc;
    }

    // --- a static NATIVE (JDK intrinsic): the MethodSiteCache actual path ---
    static int nativeKernel(int iters, int v) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += Math.abs(v); acc += Math.abs(v); acc += Math.abs(v); acc += Math.abs(v);
            acc += Math.abs(v); acc += Math.abs(v); acc += Math.abs(v); acc += Math.abs(v);
            acc += Math.abs(v); acc += Math.abs(v); acc += Math.abs(v); acc += Math.abs(v);
            acc += Math.abs(v); acc += Math.abs(v); acc += Math.abs(v); acc += Math.abs(v);
        }
        return acc;
    }
    static int nativeBase(int iters, int v) {
        int acc = 0;
        for (int i = 0; i < iters; i++) { acc += Math.abs(v); }
        return acc;
    }

    // --- control ---
    static int iaddKernel(int iters, int seed) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += seed; acc += seed; acc += seed; acc += seed;
            acc += seed; acc += seed; acc += seed; acc += seed;
            acc += seed; acc += seed; acc += seed; acc += seed;
            acc += seed; acc += seed; acc += seed; acc += seed;
        }
        return acc;
    }
    static int iaddBase(int iters, int seed) {
        int acc = 0;
        for (int i = 0; i < iters; i++) { acc += seed; }
        return acc;
    }

    static long timeNs(Runnable r) {
        long t0 = System.nanoTime();
        r.run();
        return System.nanoTime() - t0;
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        Base mono = new Sub();
        Iface one = new A();
        Iface[] four = { new A(), new B(), new C(), new D() };
        InterpInvokeCostProbe self = new InterpInvokeCostProbe();
        int[] g = new int[1];

        for (int w = 0; w < 3; w++) {
            int n = iters / 10;
            g[0] += staticKernel(n) + staticBase(n)
                  + virtMonoKernel(n, mono) + virtMonoBase(n, mono)
                  + ifaceMonoKernel(n, one) + ifaceMonoBase(n, one)
                  + ifacePolyKernel(n, four) + ifacePolyBase(n, four)
                  + finalKernel(n, self) + finalBase(n, self)
                  + nativeKernel(n, -3) + nativeBase(n, -3)
                  + iaddKernel(n, 3) + iaddBase(n, 3);
        }

        long sK = timeNs(() -> g[0] += staticKernel(iters));
        long sB = timeNs(() -> g[0] += staticBase(iters));
        long vK = timeNs(() -> g[0] += virtMonoKernel(iters, mono));
        long vB = timeNs(() -> g[0] += virtMonoBase(iters, mono));
        long iK = timeNs(() -> g[0] += ifaceMonoKernel(iters, one));
        long iB = timeNs(() -> g[0] += ifaceMonoBase(iters, one));
        long pK = timeNs(() -> g[0] += ifacePolyKernel(iters, four));
        long pB = timeNs(() -> g[0] += ifacePolyBase(iters, four));
        long fK = timeNs(() -> g[0] += finalKernel(iters, self));
        long fB = timeNs(() -> g[0] += finalBase(iters, self));
        long nK = timeNs(() -> g[0] += nativeKernel(iters, -3));
        long nB = timeNs(() -> g[0] += nativeBase(iters, -3));
        long aK = timeNs(() -> g[0] += iaddKernel(iters, 3));
        long aB = timeNs(() -> g[0] += iaddBase(iters, 3));

        long per = (long) iters * (UNROLL - 1);
        System.out.printf("invokestatic     %8.1f ns/call%n", (double) (sK - sB) / per);
        System.out.printf("invokevirtual    %8.1f ns/call%n", (double) (vK - vB) / per);
        System.out.printf("invokeinterface  %8.1f ns/call%n", (double) (iK - iB) / per);
        System.out.printf("iface-4receiver  %8.1f ns/call%n", (double) (pK - pB) / per);
        System.out.printf("final-instance   %8.1f ns/call%n", (double) (fK - fB) / per);
        System.out.printf("static-native    %8.1f ns/call%n", (double) (nK - nB) / per);
        System.out.printf("iadd(ctrl)       %8.1f ns/op%n",   (double) (aK - aB) / per);
        System.out.println("guard " + g[0]);
    }
}
