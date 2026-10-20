// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe + bench
//
// Interpreter round i6 wave 118, lane L7: `CRATONVM_INTERP_FIELD_CTOR_LDC_VALUES` (default off;
// the `ldc` leftover of
// docs/internal/proposals/interpreter/i24-L4-proposal-frameless-trivial-constructor-in-the-special-door-20260927.md).
// The special door's frameless field-store constructor also answers a numeric `ldc` / `ldc_w` /
// `ldc2_w` store, the constant read from the declaring class's pool once per method. Rows build
// objects in a loop (so the field sites are filled and the door answers) and print the fields
// bit-exact:
//
//   ints        int limit = 100_000; int neg = -40_000          ldc Integer
//   floats      float ratio = 2.5f; float tiny = 1e-30f          ldc Float
//   longs       long id = 42L; long big = Long.MIN_VALUE + 1     ldc2_w Long
//   doubles     double third = 1.0 / 3; double nan = Double.NaN; double negzero = -0.0
//   mixed       Mixed(int k) { this.k = k; } + long stamp = 1_000_000_000_000L
//   string      String name = "fixed" (control: a String constant keeps the frame)
//   many-pool   a class whose constants sit past ldc's one-byte index (ldc_w)
//
// Bench (TIME lines, --nojit): `new-ints`, `new-longs`, `new-doubles` expected down with the
// switch; `new-string` flat (control).
//
// Run, switch off and on, --jdk-only and --compatible, with and without --nojit:
//   cratonvm --java-home <jdk25> --jdk-only --nojit -cp <dir> L7W118FieldCtorLdcValues
//   CRATONVM_INTERP_FIELD_CTOR_LDC_VALUES=1 cratonvm --java-home <jdk25> --jdk-only --nojit -cp <dir> L7W118FieldCtorLdcValues
// Census: CRATONVM_DBG_FIELD_SITE=1 `[invoke-door] field-store constructors: elided=N` grows.
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// ints 100000 -40000
// floats 40200000 da24260
// longs 42 -9223372036854775807
// doubles 3fd5555555555555 7ff8000000000000 8000000000000000
// mixed 4999 1000000000000
// string fixed 70000
// many-pool 123456789 3.75 filler=8882817550726330508
// acc 617946442500
// new-ints checksum=20000000000
// new-longs checksum=8400000
// new-doubles checksum=0
// new-string checksum=14000000000
// HOTSPOT_EXPECTED_END

public class L7W118FieldCtorLdcValues {
    static final class Ints {
        int limit = 100_000;
        int neg = -40_000;
    }

    static final class Floats {
        float ratio = 2.5f;
        float tiny = 1e-30f;
    }

    static final class Longs {
        long id = 42L;
        long big = Long.MIN_VALUE + 1;
    }

    static final class Doubles {
        double third = 1.0 / 3;
        double nan = Double.NaN;
        double negzero = -0.0;
    }

    static final class Mixed {
        int k;
        long stamp = 1_000_000_000_000L;

        Mixed(int k) {
            this.k = k;
        }
    }

    static final class Named {
        String name = "fixed";
        int n = 70_000;
    }

    // A method declared before the constructor, with 280 distinct int constants: javac fills
    // the pool in code order, so the constructor's own constants sit past index 255 (ldc_w).
    static final class ManyPool {
        static long filler() {
            long s = 0;
            s = s * 31 + 100000; s = s * 31 + 100001; s = s * 31 + 100002; s = s * 31 + 100003; s = s * 31 + 100004; s = s * 31 + 100005; s = s * 31 + 100006; s = s * 31 + 100007; s = s * 31 + 100008; s = s * 31 + 100009;
            s = s * 31 + 100010; s = s * 31 + 100011; s = s * 31 + 100012; s = s * 31 + 100013; s = s * 31 + 100014; s = s * 31 + 100015; s = s * 31 + 100016; s = s * 31 + 100017; s = s * 31 + 100018; s = s * 31 + 100019;
            s = s * 31 + 100020; s = s * 31 + 100021; s = s * 31 + 100022; s = s * 31 + 100023; s = s * 31 + 100024; s = s * 31 + 100025; s = s * 31 + 100026; s = s * 31 + 100027; s = s * 31 + 100028; s = s * 31 + 100029;
            s = s * 31 + 100030; s = s * 31 + 100031; s = s * 31 + 100032; s = s * 31 + 100033; s = s * 31 + 100034; s = s * 31 + 100035; s = s * 31 + 100036; s = s * 31 + 100037; s = s * 31 + 100038; s = s * 31 + 100039;
            s = s * 31 + 100040; s = s * 31 + 100041; s = s * 31 + 100042; s = s * 31 + 100043; s = s * 31 + 100044; s = s * 31 + 100045; s = s * 31 + 100046; s = s * 31 + 100047; s = s * 31 + 100048; s = s * 31 + 100049;
            s = s * 31 + 100050; s = s * 31 + 100051; s = s * 31 + 100052; s = s * 31 + 100053; s = s * 31 + 100054; s = s * 31 + 100055; s = s * 31 + 100056; s = s * 31 + 100057; s = s * 31 + 100058; s = s * 31 + 100059;
            s = s * 31 + 100060; s = s * 31 + 100061; s = s * 31 + 100062; s = s * 31 + 100063; s = s * 31 + 100064; s = s * 31 + 100065; s = s * 31 + 100066; s = s * 31 + 100067; s = s * 31 + 100068; s = s * 31 + 100069;
            s = s * 31 + 100070; s = s * 31 + 100071; s = s * 31 + 100072; s = s * 31 + 100073; s = s * 31 + 100074; s = s * 31 + 100075; s = s * 31 + 100076; s = s * 31 + 100077; s = s * 31 + 100078; s = s * 31 + 100079;
            s = s * 31 + 100080; s = s * 31 + 100081; s = s * 31 + 100082; s = s * 31 + 100083; s = s * 31 + 100084; s = s * 31 + 100085; s = s * 31 + 100086; s = s * 31 + 100087; s = s * 31 + 100088; s = s * 31 + 100089;
            s = s * 31 + 100090; s = s * 31 + 100091; s = s * 31 + 100092; s = s * 31 + 100093; s = s * 31 + 100094; s = s * 31 + 100095; s = s * 31 + 100096; s = s * 31 + 100097; s = s * 31 + 100098; s = s * 31 + 100099;
            s = s * 31 + 100100; s = s * 31 + 100101; s = s * 31 + 100102; s = s * 31 + 100103; s = s * 31 + 100104; s = s * 31 + 100105; s = s * 31 + 100106; s = s * 31 + 100107; s = s * 31 + 100108; s = s * 31 + 100109;
            s = s * 31 + 100110; s = s * 31 + 100111; s = s * 31 + 100112; s = s * 31 + 100113; s = s * 31 + 100114; s = s * 31 + 100115; s = s * 31 + 100116; s = s * 31 + 100117; s = s * 31 + 100118; s = s * 31 + 100119;
            s = s * 31 + 100120; s = s * 31 + 100121; s = s * 31 + 100122; s = s * 31 + 100123; s = s * 31 + 100124; s = s * 31 + 100125; s = s * 31 + 100126; s = s * 31 + 100127; s = s * 31 + 100128; s = s * 31 + 100129;
            s = s * 31 + 100130; s = s * 31 + 100131; s = s * 31 + 100132; s = s * 31 + 100133; s = s * 31 + 100134; s = s * 31 + 100135; s = s * 31 + 100136; s = s * 31 + 100137; s = s * 31 + 100138; s = s * 31 + 100139;
            s = s * 31 + 100140; s = s * 31 + 100141; s = s * 31 + 100142; s = s * 31 + 100143; s = s * 31 + 100144; s = s * 31 + 100145; s = s * 31 + 100146; s = s * 31 + 100147; s = s * 31 + 100148; s = s * 31 + 100149;
            s = s * 31 + 100150; s = s * 31 + 100151; s = s * 31 + 100152; s = s * 31 + 100153; s = s * 31 + 100154; s = s * 31 + 100155; s = s * 31 + 100156; s = s * 31 + 100157; s = s * 31 + 100158; s = s * 31 + 100159;
            s = s * 31 + 100160; s = s * 31 + 100161; s = s * 31 + 100162; s = s * 31 + 100163; s = s * 31 + 100164; s = s * 31 + 100165; s = s * 31 + 100166; s = s * 31 + 100167; s = s * 31 + 100168; s = s * 31 + 100169;
            s = s * 31 + 100170; s = s * 31 + 100171; s = s * 31 + 100172; s = s * 31 + 100173; s = s * 31 + 100174; s = s * 31 + 100175; s = s * 31 + 100176; s = s * 31 + 100177; s = s * 31 + 100178; s = s * 31 + 100179;
            s = s * 31 + 100180; s = s * 31 + 100181; s = s * 31 + 100182; s = s * 31 + 100183; s = s * 31 + 100184; s = s * 31 + 100185; s = s * 31 + 100186; s = s * 31 + 100187; s = s * 31 + 100188; s = s * 31 + 100189;
            s = s * 31 + 100190; s = s * 31 + 100191; s = s * 31 + 100192; s = s * 31 + 100193; s = s * 31 + 100194; s = s * 31 + 100195; s = s * 31 + 100196; s = s * 31 + 100197; s = s * 31 + 100198; s = s * 31 + 100199;
            s = s * 31 + 100200; s = s * 31 + 100201; s = s * 31 + 100202; s = s * 31 + 100203; s = s * 31 + 100204; s = s * 31 + 100205; s = s * 31 + 100206; s = s * 31 + 100207; s = s * 31 + 100208; s = s * 31 + 100209;
            s = s * 31 + 100210; s = s * 31 + 100211; s = s * 31 + 100212; s = s * 31 + 100213; s = s * 31 + 100214; s = s * 31 + 100215; s = s * 31 + 100216; s = s * 31 + 100217; s = s * 31 + 100218; s = s * 31 + 100219;
            s = s * 31 + 100220; s = s * 31 + 100221; s = s * 31 + 100222; s = s * 31 + 100223; s = s * 31 + 100224; s = s * 31 + 100225; s = s * 31 + 100226; s = s * 31 + 100227; s = s * 31 + 100228; s = s * 31 + 100229;
            s = s * 31 + 100230; s = s * 31 + 100231; s = s * 31 + 100232; s = s * 31 + 100233; s = s * 31 + 100234; s = s * 31 + 100235; s = s * 31 + 100236; s = s * 31 + 100237; s = s * 31 + 100238; s = s * 31 + 100239;
            s = s * 31 + 100240; s = s * 31 + 100241; s = s * 31 + 100242; s = s * 31 + 100243; s = s * 31 + 100244; s = s * 31 + 100245; s = s * 31 + 100246; s = s * 31 + 100247; s = s * 31 + 100248; s = s * 31 + 100249;
            s = s * 31 + 100250; s = s * 31 + 100251; s = s * 31 + 100252; s = s * 31 + 100253; s = s * 31 + 100254; s = s * 31 + 100255; s = s * 31 + 100256; s = s * 31 + 100257; s = s * 31 + 100258; s = s * 31 + 100259;
            s = s * 31 + 100260; s = s * 31 + 100261; s = s * 31 + 100262; s = s * 31 + 100263; s = s * 31 + 100264; s = s * 31 + 100265; s = s * 31 + 100266; s = s * 31 + 100267; s = s * 31 + 100268; s = s * 31 + 100269;
            s = s * 31 + 100270; s = s * 31 + 100271; s = s * 31 + 100272; s = s * 31 + 100273; s = s * 31 + 100274; s = s * 31 + 100275; s = s * 31 + 100276; s = s * 31 + 100277; s = s * 31 + 100278; s = s * 31 + 100279;
            return s;
        }

        int wide;
        float wideF;

        ManyPool() {
            this.wide = 123_456_789;
            this.wideF = 3.75f;
        }
    }

    static final int ROUNDS = 5_000;

    public static void main(String[] args) {
        Ints a = null;
        Floats b = null;
        Longs c = null;
        Doubles d = null;
        Mixed m = null;
        Named s = null;
        ManyPool p = null;
        long acc = 0;
        for (int i = 0; i < ROUNDS; i++) {
            a = new Ints();
            b = new Floats();
            c = new Longs();
            d = new Doubles();
            m = new Mixed(i);
            s = new Named();
            p = new ManyPool();
            acc += a.limit + a.neg + m.k + s.n + p.wide;
        }
        System.out.println("ints " + a.limit + " " + a.neg);
        System.out.println("floats " + Integer.toHexString(Float.floatToRawIntBits(b.ratio)) + " "
                + Integer.toHexString(Float.floatToRawIntBits(b.tiny)));
        System.out.println("longs " + c.id + " " + c.big);
        System.out.println("doubles " + Long.toHexString(Double.doubleToRawLongBits(d.third)) + " "
                + Long.toHexString(Double.doubleToRawLongBits(d.nan)) + " "
                + Long.toHexString(Double.doubleToRawLongBits(d.negzero)));
        System.out.println("mixed " + m.k + " " + m.stamp);
        System.out.println("string " + s.name + " " + s.n);
        System.out.println("many-pool " + p.wide + " " + p.wideF + " filler=" + ManyPool.filler());
        System.out.println("acc " + acc);
        bench();
    }

    interface Row {
        long run(int n);
    }

    static void bench() {
        String[] names = {"new-ints", "new-longs", "new-doubles", "new-string"};
        Row[] rows = {
            n -> {
                long t = 0;
                for (int i = 0; i < n; i++) {
                    t += new Ints().limit;
                }
                return t;
            },
            n -> {
                long t = 0;
                for (int i = 0; i < n; i++) {
                    t += new Longs().id;
                }
                return t;
            },
            n -> {
                long t = 0;
                for (int i = 0; i < n; i++) {
                    t += (long) new Doubles().third;
                }
                return t;
            },
            n -> {
                long t = 0;
                for (int i = 0; i < n; i++) {
                    t += new Named().n;
                }
                return t;
            },
        };
        final int n = 200_000;
        for (int r = 0; r < rows.length; r++) {
            long checksum = rows[r].run(n);
            long best = Long.MAX_VALUE;
            for (int rep = 0; rep < 3; rep++) {
                long t0 = System.nanoTime();
                rows[r].run(n);
                best = Math.min(best, System.nanoTime() - t0);
            }
            System.out.println(names[r] + " checksum=" + checksum);
            System.out.println("TIME " + names[r] + " " + (best / n) + " ns/op (min of 3)");
        }
    }
}
