// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Does `areturn` cost more than `ireturn`, and if so, is it the descriptor scan?
//
// The return arm does extra work for `areturn` (0xb0) only:
//
//     let ret = crate::jit::return_type(frame.method_descriptor());
//     coerce_value_for_return_validated(shared, cv.to_value(), ret)
//
// `return_type` linearly scans the descriptor for ')', on every reference
// return. The i/l/f/d returns were already fixed to copy the raw CompactValue
// and skip the to_value()/from_value() round trip; `areturn` was deliberately
// left on it "to normalize jobject-as-Long handles". The return path was
// measured on 2026-08-19 as the LARGEST phase of an interpreted call — but with
// a probe returning `int`, so this arm has never been measured.
//
// TWO questions, and the second is what makes this a diagnosis:
//   1. Is the reference return measurably dearer than the int return?
//   2. Does the gap GROW with descriptor length? The scan runs to ')', ~1 char
//      on the short descriptor and ~145 on the long one. A FLAT gap acquits the
//      scan and convicts the to_value/coerce/push round trip, which needs a
//      completely different fix. Conflating them would "fix" the wrong half.
//
// ── Everything below exists because three earlier versions of this probe
//    measured something other than the return. ──────────────────────────────
//
//   1. CONSUMER. `a += iShort()` (iadd) against `if (aShort() != null)`
//      (ifnonnull) measures the consuming opcode as much as the return.
//      Both arms now consume with a one-operand branch: ifeq vs ifnull.
//   2. CALLEE BODY. `iShort() { return 1; }` (iconst_1) against
//      `aShort() { return SINK; }` (GETSTATIC) — getstatic costs ~161ns alone
//      in this interpreter and accounted for the entire reported gap. Both
//      callees are now parameter passthrough: iload_0/ireturn, aload_0/areturn.
//   3. CALLER ARGUMENT. Passing `SINK` re-introduced the same getstatic in the
//      caller instead. Both arguments now come from a LOCAL hoisted out of the
//      loop: iload against aload.
//
// The lesson each time was the same one: when two arms differ in more than the
// thing under test, the difference is attributed to whatever you are looking
// for. Check the bytecode, not the source.
public final class AreturnCostProbe {

    static final int UNROLL = 16;
    static final Object SINK = new Object();
    static final String S = "s";

    // Callee bodies, identical apart from the return opcode.
    static int iShort(int x) { return x; }
    static Object aShort(Object x) { return x; }

    // Same pair with a ~150-char descriptor, so `return_type`'s scan is ~145x
    // longer. Each is differenced against its OWN base loop, so the eight
    // argument pushes cancel and only the return-kind difference survives.
    static int iLong(int x, String b, String c, String d,
                     String e, String f, String g, String h) { return x; }
    static Object aLong(Object x, String b, String c, String d,
                        String e, String f, String g, String h) { return x; }

    static int kIShort(int n, int one){int a=0;for(int i=0;i<n;i++){if(iShort(one)!=0)a++;if(iShort(one)!=0)a++;if(iShort(one)!=0)a++;if(iShort(one)!=0)a++;if(iShort(one)!=0)a++;if(iShort(one)!=0)a++;if(iShort(one)!=0)a++;if(iShort(one)!=0)a++;if(iShort(one)!=0)a++;if(iShort(one)!=0)a++;if(iShort(one)!=0)a++;if(iShort(one)!=0)a++;if(iShort(one)!=0)a++;if(iShort(one)!=0)a++;if(iShort(one)!=0)a++;if(iShort(one)!=0)a++;}return a;}
    static int bIShort(int n, int one){int a=0;for(int i=0;i<n;i++){if(iShort(one)!=0)a++;}return a;}

    static int kAShort(int n, Object o){int a=0;for(int i=0;i<n;i++){if(aShort(o)!=null)a++;if(aShort(o)!=null)a++;if(aShort(o)!=null)a++;if(aShort(o)!=null)a++;if(aShort(o)!=null)a++;if(aShort(o)!=null)a++;if(aShort(o)!=null)a++;if(aShort(o)!=null)a++;if(aShort(o)!=null)a++;if(aShort(o)!=null)a++;if(aShort(o)!=null)a++;if(aShort(o)!=null)a++;if(aShort(o)!=null)a++;if(aShort(o)!=null)a++;if(aShort(o)!=null)a++;if(aShort(o)!=null)a++;}return a;}
    static int bAShort(int n, Object o){int a=0;for(int i=0;i<n;i++){if(aShort(o)!=null)a++;}return a;}

    static int kILong(int n, int one, String s){int a=0;for(int i=0;i<n;i++){if(iLong(one,s,s,s,s,s,s,s)!=0)a++;if(iLong(one,s,s,s,s,s,s,s)!=0)a++;if(iLong(one,s,s,s,s,s,s,s)!=0)a++;if(iLong(one,s,s,s,s,s,s,s)!=0)a++;if(iLong(one,s,s,s,s,s,s,s)!=0)a++;if(iLong(one,s,s,s,s,s,s,s)!=0)a++;if(iLong(one,s,s,s,s,s,s,s)!=0)a++;if(iLong(one,s,s,s,s,s,s,s)!=0)a++;if(iLong(one,s,s,s,s,s,s,s)!=0)a++;if(iLong(one,s,s,s,s,s,s,s)!=0)a++;if(iLong(one,s,s,s,s,s,s,s)!=0)a++;if(iLong(one,s,s,s,s,s,s,s)!=0)a++;if(iLong(one,s,s,s,s,s,s,s)!=0)a++;if(iLong(one,s,s,s,s,s,s,s)!=0)a++;if(iLong(one,s,s,s,s,s,s,s)!=0)a++;if(iLong(one,s,s,s,s,s,s,s)!=0)a++;}return a;}
    static int bILong(int n, int one, String s){int a=0;for(int i=0;i<n;i++){if(iLong(one,s,s,s,s,s,s,s)!=0)a++;}return a;}

    static int kALong(int n, Object o, String s){int a=0;for(int i=0;i<n;i++){if(aLong(o,s,s,s,s,s,s,s)!=null)a++;if(aLong(o,s,s,s,s,s,s,s)!=null)a++;if(aLong(o,s,s,s,s,s,s,s)!=null)a++;if(aLong(o,s,s,s,s,s,s,s)!=null)a++;if(aLong(o,s,s,s,s,s,s,s)!=null)a++;if(aLong(o,s,s,s,s,s,s,s)!=null)a++;if(aLong(o,s,s,s,s,s,s,s)!=null)a++;if(aLong(o,s,s,s,s,s,s,s)!=null)a++;if(aLong(o,s,s,s,s,s,s,s)!=null)a++;if(aLong(o,s,s,s,s,s,s,s)!=null)a++;if(aLong(o,s,s,s,s,s,s,s)!=null)a++;if(aLong(o,s,s,s,s,s,s,s)!=null)a++;if(aLong(o,s,s,s,s,s,s,s)!=null)a++;if(aLong(o,s,s,s,s,s,s,s)!=null)a++;if(aLong(o,s,s,s,s,s,s,s)!=null)a++;if(aLong(o,s,s,s,s,s,s,s)!=null)a++;}return a;}
    static int bALong(int n, Object o, String s){int a=0;for(int i=0;i<n;i++){if(aLong(o,s,s,s,s,s,s,s)!=null)a++;}return a;}

    static int kAdd(int n,int s){int a=0;for(int i=0;i<n;i++){a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;a+=s;}return a;}
    static int bAdd(int n,int s){int a=0;for(int i=0;i<n;i++){a+=s;}return a;}

    static long t(Runnable r){long x=System.nanoTime();r.run();return System.nanoTime()-x;}

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 150_000;
        int R = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        int[] g = new int[1];
        final int one = args.length > 2 ? 1 : 1;
        final Object o = SINK;
        final String s = S;

        for (int w = 0; w < 3; w++) {
            int m = n/10;
            g[0]+=kIShort(m,one)+bIShort(m,one)+kAShort(m,o)+bAShort(m,o)
                 +kILong(m,one,s)+bILong(m,one,s)+kALong(m,o,s)+bALong(m,o,s)
                 +kAdd(m,3)+bAdd(m,3);
        }

        long isK=0,isB=0,asK=0,asB=0,alK=0,alB=0,ilK=0,ilB=0,adK=0,adB=0;
        for (int r = 0; r < R; r++) {
            isK += t(()->g[0]+=kIShort(n,one));   isB += t(()->g[0]+=bIShort(n,one));
            asK += t(()->g[0]+=kAShort(n,o));     asB += t(()->g[0]+=bAShort(n,o));
            ilK += t(()->g[0]+=kILong(n,one,s));  ilB += t(()->g[0]+=bILong(n,one,s));
            alK += t(()->g[0]+=kALong(n,o,s));    alB += t(()->g[0]+=bALong(n,o,s));
            long x = t(()->g[0]+=kAdd(n,3)), y = t(()->g[0]+=bAdd(n,3));
            adK += x; adB += y;
            System.out.printf("  round %d control %6.1f ns/op%n", r,
                    (double)(x-y) / ((long) n * (UNROLL-1)));
        }

        long per = (long) n * (UNROLL - 1) * R;
        double is=(double)(isK-isB)/per, as=(double)(asK-asB)/per;
        double il=(double)(ilK-ilB)/per, al=(double)(alK-alB)/per;
        System.out.printf("ireturn  short desc   %8.1f ns/call%n", is);
        System.out.printf("areturn  short desc   %8.1f ns/call   gap %+7.1f%n", as, as-is);
        System.out.printf("ireturn  LONG desc    %8.1f ns/call%n", il);
        System.out.printf("areturn  LONG desc    %8.1f ns/call   gap %+7.1f%n", al, al-il);
        System.out.printf("iadd (control)        %8.1f ns/op%n",(double)(adK-adB)/per);
        System.out.println("guard "+g[0]);
    }
}
