// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// WHERE does the cost of a CratonVM interpreter call go?
//
// InterpInvokeCostProbe established the size of the problem: against HotSpot's
// interpreter CratonVM runs iadd at 5.9x but invocations at 12-29x, so calls
// are disproportionately expensive rather than uniformly slow. It could not say
// WHY. Critically, invokestatic -- no receiver, no vtable, no inline-cache
// receiver guard -- was already 170ns against HotSpot's 14.4ns. Subtracting the
// callee body (2 opcodes) leaves ~126ns for dispatch plus frame push plus frame
// pop. That cost is shared by every invoke kind, which makes it a bigger target
// than anything on the virtual-dispatch path alone.
//
// This probe decomposes that ~126ns by varying ONE property of the callee at a
// time and holding the call site identical. Each hypothesis names the code it
// would convict:
//
//   H1  LOCALS COUNT -- `Frame::new_pooled_cached` calls `init_locals_pooled`,
//       which must size and initialise `locals` + `local_kinds` up to
//       `max_locals` on every push. If the cost scales with declared locals,
//       that loop is the driver. `deep()` declares ~60 live locals; `flat()`
//       declares one. Both take 0 args and return a constant.
//
//   H2  ARGUMENT COUNT -- argument popping and coercion runs per call
//       (`pop_coerced_invoke_args_*`). If cost scales with arity, marshalling
//       is the driver rather than the frame itself.
//
//   H3  OPERAND STACK DEPTH -- the pooled `ValueStack` is sized
//       `max(max_stack,16)+8` per push. `wide()` forces a deep operand stack in
//       the callee without adding locals or args.
//
//   H4  FIXED FRAME COST -- if none of the above move, what remains is
//       per-push work independent of the method's shape: the two Arc refcount
//       bumps (`code.clone()` and the cached-method Arc), the ~250-byte Frame
//       move into the frame stack, and the pool pop/push pair. That would be
//       the thing to attack, and it would say the fix is structural rather
//       than a loop bound.
//
// A depth-2 chain (`callsCallee`) is included because a call that itself calls
// exercises push-on-top-of-push rather than push-into-a-warm-slot.
//
// METHOD. Arms are INTERLEAVED within each round and the per-round control is
// printed. An earlier probe in this tree ran its arms sequentially and reported
// a 3.8x effect that was entirely host-load drift landing on the arms measured
// last; that claim had to be withdrawn. Discard any round whose control departs
// from its neighbours rather than reading its arms.
public final class InvokeFrameCostProbe {

    static final int UNROLL = 16;

    // ---- H1: locals count. Both take no args, return a constant. ----
    static int flat() { return 1; }

    static int deep() {
        int a00=1,a01=1,a02=1,a03=1,a04=1,a05=1,a06=1,a07=1,a08=1,a09=1;
        int a10=1,a11=1,a12=1,a13=1,a14=1,a15=1,a16=1,a17=1,a18=1,a19=1;
        int a20=1,a21=1,a22=1,a23=1,a24=1,a25=1,a26=1,a27=1,a28=1,a29=1;
        int a30=1,a31=1,a32=1,a33=1,a34=1,a35=1,a36=1,a37=1,a38=1,a39=1;
        int a40=1,a41=1,a42=1,a43=1,a44=1,a45=1,a46=1,a47=1,a48=1,a49=1;
        int a50=1,a51=1,a52=1,a53=1,a54=1,a55=1,a56=1,a57=1,a58=1,a59=1;
        // One use so javac cannot drop the slots; the ADD chain is the same
        // work in both arms only in the sense that it is small and constant --
        // the arms are differenced against their OWN base loop, so the callee
        // body cancels and only the per-call frame cost remains.
        return a00+a09+a19+a29+a39+a49+a59;
    }

    // ---- H2: argument count ----
    static int args0() { return 1; }
    static int args4(int a, int b, int c, int d) { return a; }
    static int args8(int a, int b, int c, int d, int e, int f, int g, int h) { return a; }

    // ---- H3: operand stack depth, no extra locals or args ----
    //
    // Sourced from a NON-FINAL static, not from literals. Written with
    // constants -- `((((((((1+2)*3)+4)*5)+6)*7)+8)*9)` -- javac folds the whole
    // chain and emits `stack=1`, so the arm measures nothing and the row reads
    // as a null result for H3 rather than as a broken probe. Checked with
    // `javap -v` (the arm must report a stack depth well above 1).
    static int seed = 2;

    static int wide() {
        int s = seed;
        return (s+s)*(s+s)*(s+s) + (s+s)*(s+s)*(s+s)
             + (s+s)*(s+s)*(s+s) + (s+s)*(s+s)*(s+s);
    }

    // ---- depth-2 chain ----
    static int leaf() { return 1; }
    static int callsCallee() { return leaf(); }

    static int kFlat(int n){int a=0;for(int i=0;i<n;i++){a+=flat();a+=flat();a+=flat();a+=flat();a+=flat();a+=flat();a+=flat();a+=flat();a+=flat();a+=flat();a+=flat();a+=flat();a+=flat();a+=flat();a+=flat();a+=flat();}return a;}
    static int bFlat(int n){int a=0;for(int i=0;i<n;i++){a+=flat();}return a;}

    static int kDeep(int n){int a=0;for(int i=0;i<n;i++){a+=deep();a+=deep();a+=deep();a+=deep();a+=deep();a+=deep();a+=deep();a+=deep();a+=deep();a+=deep();a+=deep();a+=deep();a+=deep();a+=deep();a+=deep();a+=deep();}return a;}
    static int bDeep(int n){int a=0;for(int i=0;i<n;i++){a+=deep();}return a;}

    static int kA0(int n){int a=0;for(int i=0;i<n;i++){a+=args0();a+=args0();a+=args0();a+=args0();a+=args0();a+=args0();a+=args0();a+=args0();a+=args0();a+=args0();a+=args0();a+=args0();a+=args0();a+=args0();a+=args0();a+=args0();}return a;}
    static int bA0(int n){int a=0;for(int i=0;i<n;i++){a+=args0();}return a;}

    static int kA4(int n){int a=0;for(int i=0;i<n;i++){a+=args4(1,2,3,4);a+=args4(1,2,3,4);a+=args4(1,2,3,4);a+=args4(1,2,3,4);a+=args4(1,2,3,4);a+=args4(1,2,3,4);a+=args4(1,2,3,4);a+=args4(1,2,3,4);a+=args4(1,2,3,4);a+=args4(1,2,3,4);a+=args4(1,2,3,4);a+=args4(1,2,3,4);a+=args4(1,2,3,4);a+=args4(1,2,3,4);a+=args4(1,2,3,4);a+=args4(1,2,3,4);}return a;}
    static int bA4(int n){int a=0;for(int i=0;i<n;i++){a+=args4(1,2,3,4);}return a;}

    static int kA8(int n){int a=0;for(int i=0;i<n;i++){a+=args8(1,2,3,4,5,6,7,8);a+=args8(1,2,3,4,5,6,7,8);a+=args8(1,2,3,4,5,6,7,8);a+=args8(1,2,3,4,5,6,7,8);a+=args8(1,2,3,4,5,6,7,8);a+=args8(1,2,3,4,5,6,7,8);a+=args8(1,2,3,4,5,6,7,8);a+=args8(1,2,3,4,5,6,7,8);a+=args8(1,2,3,4,5,6,7,8);a+=args8(1,2,3,4,5,6,7,8);a+=args8(1,2,3,4,5,6,7,8);a+=args8(1,2,3,4,5,6,7,8);a+=args8(1,2,3,4,5,6,7,8);a+=args8(1,2,3,4,5,6,7,8);a+=args8(1,2,3,4,5,6,7,8);a+=args8(1,2,3,4,5,6,7,8);}return a;}
    static int bA8(int n){int a=0;for(int i=0;i<n;i++){a+=args8(1,2,3,4,5,6,7,8);}return a;}

    static int kWide(int n){int a=0;for(int i=0;i<n;i++){a+=wide();a+=wide();a+=wide();a+=wide();a+=wide();a+=wide();a+=wide();a+=wide();a+=wide();a+=wide();a+=wide();a+=wide();a+=wide();a+=wide();a+=wide();a+=wide();}return a;}
    static int bWide(int n){int a=0;for(int i=0;i<n;i++){a+=wide();}return a;}

    static int kChain(int n){int a=0;for(int i=0;i<n;i++){a+=callsCallee();a+=callsCallee();a+=callsCallee();a+=callsCallee();a+=callsCallee();a+=callsCallee();a+=callsCallee();a+=callsCallee();a+=callsCallee();a+=callsCallee();a+=callsCallee();a+=callsCallee();a+=callsCallee();a+=callsCallee();a+=callsCallee();a+=callsCallee();}return a;}
    static int bChain(int n){int a=0;for(int i=0;i<n;i++){a+=callsCallee();}return a;}

    static int kAdd(int n,int s){int a=0;for(int i=0;i<n;i++){a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;}return a;}
    static int bAdd(int n,int s){int a=0;for(int i=0;i<n;i++){a+=s;}return a;}

    static long t(Runnable r){long x=System.nanoTime();r.run();return System.nanoTime()-x;}

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 120_000;
        int R = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        int[] g = new int[1];

        for (int w = 0; w < 3; w++) {
            int m = n/10;
            g[0]+=kFlat(m)+bFlat(m)+kDeep(m)+bDeep(m)+kA0(m)+bA0(m)+kA4(m)+bA4(m)
                 +kA8(m)+bA8(m)+kWide(m)+bWide(m)+kChain(m)+bChain(m)+kAdd(m,3)+bAdd(m,3);
        }

        long fK=0,fB=0,dK=0,dB=0,a0K=0,a0B=0,a4K=0,a4B=0,a8K=0,a8B=0,
             wK=0,wB=0,cK=0,cB=0,aK=0,aB=0;
        for (int r = 0; r < R; r++) {
            fK += t(()->g[0]+=kFlat(n));   fB += t(()->g[0]+=bFlat(n));
            dK += t(()->g[0]+=kDeep(n));   dB += t(()->g[0]+=bDeep(n));
            a0K+= t(()->g[0]+=kA0(n));     a0B+= t(()->g[0]+=bA0(n));
            a4K+= t(()->g[0]+=kA4(n));     a4B+= t(()->g[0]+=bA4(n));
            a8K+= t(()->g[0]+=kA8(n));     a8B+= t(()->g[0]+=bA8(n));
            wK += t(()->g[0]+=kWide(n));   wB += t(()->g[0]+=bWide(n));
            cK += t(()->g[0]+=kChain(n));  cB += t(()->g[0]+=bChain(n));
            long x = t(()->g[0]+=kAdd(n,3)), y = t(()->g[0]+=bAdd(n,3));
            aK += x; aB += y;
            System.out.printf("  round %d control %6.1f ns/op%n", r,
                    (double)(x-y) / ((long) n * (UNROLL-1)));
        }

        long per = (long) n * (UNROLL - 1) * R;
        System.out.printf("H1 flat callee   (1 local)   %8.1f ns/call%n",(double)(fK-fB)/per);
        System.out.printf("H1 deep callee  (60 locals)  %8.1f ns/call%n",(double)(dK-dB)/per);
        System.out.printf("H2 args 0                    %8.1f ns/call%n",(double)(a0K-a0B)/per);
        System.out.printf("H2 args 4                    %8.1f ns/call%n",(double)(a4K-a4B)/per);
        System.out.printf("H2 args 8                    %8.1f ns/call%n",(double)(a8K-a8B)/per);
        System.out.printf("H3 wide operand stack        %8.1f ns/call%n",(double)(wK-wB)/per);
        System.out.printf("   depth-2 chain (2 pushes)  %8.1f ns/call%n",(double)(cK-cB)/per);
        System.out.printf("   iadd (control)            %8.1f ns/op%n",(double)(aK-aB)/per);
        System.out.println("guard "+g[0]);
    }
}
