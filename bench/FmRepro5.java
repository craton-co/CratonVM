// Faithful standalone copy of commons-math3 3.6.1 FastMath's sine family
// (sinQ / cosQ / polySine / polyCosine / sin + CodyWaite + the trig tables),
// for reproducing the CratonVM JIT miscompile outside the banned class name.
// See docs/gaps/gap-jit-fastmath-transform-miscompile.md Bug 3.
public final class FmRepro5 {
    // CP padding: first method in the class references 90 distinct double
    // literals so they occupy the low constant-pool indices (each Double entry
    // takes TWO slots), pushing the trig coefficients past index ~190 like the
    // real FastMath pool.
    static double cpPad() {
        double s = 0.0;
        s += 7.013E2;
        s += 7.113E3;
        s += 7.213E4;
        s += 7.313E5;
        s += 7.413E6;
        s += 7.513E7;
        s += 7.613E8;
        s += 7.713E9;
        s += 7.813E10;
        s += 7.913E11;
        s += 7.1013E12;
        s += 7.1113E13;
        s += 7.1213E14;
        s += 7.1313E15;
        s += 7.1413E16;
        s += 7.1513E17;
        s += 7.1613E18;
        s += 7.1713E19;
        s += 7.1813E20;
        s += 7.1913E21;
        s += 7.2013E22;
        s += 7.2113E23;
        s += 7.2213E24;
        s += 7.2313E25;
        s += 7.2413E26;
        s += 7.2513E27;
        s += 7.2613E28;
        s += 7.2713E29;
        s += 7.2813E2;
        s += 7.2913E3;
        s += 7.3013E4;
        s += 7.3113E5;
        s += 7.3213E6;
        s += 7.3313E7;
        s += 7.3413E8;
        s += 7.3513E9;
        s += 7.3613E10;
        s += 7.3713E11;
        s += 7.3813E12;
        s += 7.3913E13;
        s += 7.4013E14;
        s += 7.4113E15;
        s += 7.4213E16;
        s += 7.4313E17;
        s += 7.4413E18;
        s += 7.4513E19;
        s += 7.4613E20;
        s += 7.4713E21;
        s += 7.4813E22;
        s += 7.4913E23;
        s += 7.5013E24;
        s += 7.5113E25;
        s += 7.5213E26;
        s += 7.5313E27;
        s += 7.5413E28;
        s += 7.5513E29;
        s += 7.5613E2;
        s += 7.5713E3;
        s += 7.5813E4;
        s += 7.5913E5;
        s += 7.6013E6;
        s += 7.6113E7;
        s += 7.6213E8;
        s += 7.6313E9;
        s += 7.6413E10;
        s += 7.6513E11;
        s += 7.6613E12;
        s += 7.6713E13;
        s += 7.6813E14;
        s += 7.6913E15;
        s += 7.7013E16;
        s += 7.7113E17;
        s += 7.7213E18;
        s += 7.7313E19;
        s += 7.7413E20;
        s += 7.7513E21;
        s += 7.7613E22;
        s += 7.7713E23;
        s += 7.7813E24;
        s += 7.7913E25;
        s += 7.8013E26;
        s += 7.8113E27;
        s += 7.8213E28;
        s += 7.8313E29;
        s += 7.8413E2;
        s += 7.8513E3;
        s += 7.8613E4;
        s += 7.8713E5;
        s += 7.8813E6;
        s += 7.8913E7;
        return s;
    }


    /** Sine, Cosine, Tangent tables are for 0, 1/8, 2/8, ... 13/8 = PI/2 approx. */
    private static final int SINE_TABLE_LEN = 14;

    /** Sine table (high bits). */
    private static final double SINE_TABLE_A[] =
        {
        +0.0d,
        +0.1246747374534607d,
        +0.24740394949913025d,
        +0.366272509098053d,
        +0.4794255495071411d,
        +0.5850973129272461d,
        +0.6816387176513672d,
        +0.7675435543060303d,
        +0.8414709568023682d,
        +0.902267575263977d,
        +0.9489846229553223d,
        +0.9808930158615112d,
        +0.9974949359893799d,
        +0.9985313415527344d,
    };

    /** Sine table (low bits). */
    private static final double SINE_TABLE_B[] =
        {
        +0.0d,
        -4.068233003401932E-9d,
        +9.755392680573412E-9d,
        +1.9987994582857286E-8d,
        -1.0902938113007961E-8d,
        -3.9986783938944604E-8d,
        +4.23719669792332E-8d,
        -5.207000323380292E-8d,
        +2.800552834259E-8d,
        +1.883511811213715E-8d,
        -3.5997360512765566E-9d,
        +4.116164446561962E-8d,
        +5.0614674548127384E-8d,
        -1.0129027912496858E-9d,
    };

    /** Cosine table (high bits). */
    private static final double COSINE_TABLE_A[] =
        {
        +1.0d,
        +0.9921976327896118d,
        +0.9689123630523682d,
        +0.9305076599121094d,
        +0.8775825500488281d,
        +0.8109631538391113d,
        +0.7316888570785522d,
        +0.6409968137741089d,
        +0.5403022766113281d,
        +0.4311765432357788d,
        +0.3153223395347595d,
        +0.19454771280288696d,
        +0.07073719799518585d,
        -0.05417713522911072d,
    };

    /** Cosine table (low bits). */
    private static final double COSINE_TABLE_B[] =
        {
        +0.0d,
        +3.4439717236742845E-8d,
        +5.865827662008209E-8d,
        -3.7999795083850525E-8d,
        +1.184154459111628E-8d,
        -3.43338934259355E-8d,
        +1.1795268640216787E-8d,
        +4.438921624363781E-8d,
        +2.925681159240093E-8d,
        -2.6437112632041807E-8d,
        +2.2860509143963117E-8d,
        -4.813899778443457E-9d,
        +3.6725170580355583E-9d,
        +2.0217439756338078E-10d,
    };

    /** Eighths. */
    private static final double EIGHTHS[] = {0, 0.125, 0.25, 0.375, 0.5, 0.625, 0.75, 0.875, 1.0, 1.125, 1.25, 1.375, 1.5, 1.625};

    private static final long HEX_40000000 = 0x40000000L; // 1073741824L

    /**
     *  Computes sin(x) - x, where |x| < 1/16.
     *  Use a Remez polynomial approximation.
     *  @param x a number smaller than 1/16
     *  @return sin(x) - x
     */
    private static double polySine(final double x)
    {
        double x2 = x*x;

        double p = 2.7553817452272217E-6;
        p = p * x2 + -1.9841269659586505E-4;
        p = p * x2 + 0.008333333333329196;
        p = p * x2 + -0.16666666666666666;
        //p *= x2;
        //p *= x;
        p = p * x2 * x;

        return p;
    }

    /**
     *  Computes cos(x) - 1, where |x| < 1/16.
     *  Use a Remez polynomial approximation.
     *  @param x a number smaller than 1/16
     *  @return cos(x) - 1
     */
    private static double polyCosine(double x) {
        double x2 = x*x;

        double p = 2.479773539153719E-5;
        p = p * x2 + -0.0013888888689039883;
        p = p * x2 + 0.041666666666621166;
        p = p * x2 + -0.49999999999999994;
        p *= x2;

        return p;
    }

    /**
     *  Compute sine over the first quadrant (0 < x < pi/2).
     *  Use combination of table lookup and rational polynomial expansion.
     *  @param xa number from which sine is requested
     *  @param xb extra bits for x (may be 0.0)
     *  @return sin(xa + xb)
     */
    private static double sinQ(double xa, double xb) {
        int idx = (int) ((xa * 8.0) + 0.5);
        final double epsilon = xa - EIGHTHS[idx]; //idx*0.125;

        // Table lookups
        final double sintA = SINE_TABLE_A[idx];
        final double sintB = SINE_TABLE_B[idx];
        final double costA = COSINE_TABLE_A[idx];
        final double costB = COSINE_TABLE_B[idx];

        // Polynomial eval of sin(epsilon), cos(epsilon)
        double sinEpsA = epsilon;
        double sinEpsB = polySine(epsilon);
        // The real (old-javac) FastMath.class emits `dconst_1; dstore 19` for
        // cosEpsA even though every read is constant-folded. JDK25 javac elides
        // the dead constant local entirely, shifting all later slots by 2. This
        // non-final pad reproduces the real slot layout (19 double locals,
        // max_locals=39) byte-for-byte.
        double cosEpsAslot = 1.0;
        final double cosEpsA = 1.0;
        final double cosEpsB = polyCosine(epsilon);

        // Split epsilon   xa + xb = x
        final double temp = sinEpsA * HEX_40000000;
        double temp2 = (sinEpsA + temp) - temp;
        sinEpsB +=  sinEpsA - temp2;
        sinEpsA = temp2;

        /* Compute sin(x) by angle addition formula */
        double result;

        double a = 0;
        double b = 0;

        double t = sintA;
        double c = a + t;
        double d = -(c - a - t);
        a = c;
        b += d;

        t = costA * sinEpsA;
        c = a + t;
        d = -(c - a - t);
        a = c;
        b += d;

        b = b + sintA * cosEpsB + costA * sinEpsB;

        b = b + sintB + costB * sinEpsA + sintB * cosEpsB + costB * sinEpsB;

        if (xb != 0.0) {
            t = ((costA + costB) * (cosEpsA + cosEpsB) -
                 (sintA + sintB) * (sinEpsA + sinEpsB)) * xb;  // approximate cosine*xb
            c = a + t;
            d = -(c - a - t);
            a = c;
            b += d;
        }

        result = a + b;

        return result;
    }

    /**
     * Compute cosine in the first quadrant by subtracting input from PI/2 and
     * then calling sinQ.  This is more accurate as the input approaches PI/2.
     *  @param xa number from which cosine is requested
     *  @param xb extra bits for x (may be 0.0)
     *  @return cos(xa + xb)
     */
    private static double cosQ(double xa, double xb) {
        final double pi2a = 1.5707963267948966;
        final double pi2b = 6.123233995736766E-17;

        final double a = pi2a - xa;
        double b = -(a - pi2a + xa);
        b += pi2b - xb;

        return sinQ(a, b);
    }

    /** Stub: repro inputs are < 3294198.0 so PayneHanek never runs. */
    private static void reducePayneHanek(double x, double[] result) {
        throw new IllegalStateException("PayneHanek not needed for repro inputs");
    }

    private static class CodyWaite {
        /** k */
        private final int finalK;
        /** remA */
        private final double finalRemA;
        /** remB */
        private final double finalRemB;

        /**
         * @param xa Argument.
         */
        CodyWaite(double xa) {
            // Estimate k.
            //k = (int)(xa / 1.5707963267948966);
            int k = (int)(xa * 0.6366197723675814);

            // Compute remainder.
            double remA;
            double remB;
            while (true) {
                double a = -k * 1.570796251296997;
                remA = xa + a;
                remB = -(remA - xa - a);

                a = -k * 7.549789948768648E-8;
                double b = remA;
                remA = a + b;
                remB += -(remA - b - a);

                a = -k * 6.123233995736766E-17;
                b = remA;
                remA = a + b;
                remB += -(remA - b - a);

                if (remA > 0) {
                    break;
                }

                // Remainder is negative, so decrement k and try again.
                // This should only happen if the input is very close
                // to an even multiple of pi/2.
                --k;
            }

            this.finalK = k;
            this.finalRemA = remA;
            this.finalRemB = remB;
        }

        /**
         * @return k
         */
        int getK() {
            return finalK;
        }
        /**
         * @return remA
         */
        double getRemA() {
            return finalRemA;
        }
        /**
         * @return remB
         */
        double getRemB() {
            return finalRemB;
        }
    }

    /**
     * Sine function.
     *
     * @param x Argument.
     * @return sin(x)
     */
    public static double sin(double x) {
        boolean negative = false;
        int quadrant = 0;
        double xa;
        double xb = 0.0;

        /* Take absolute value of the input */
        xa = x;
        if (x < 0) {
            negative = true;
            xa = -xa;
        }

        /* Check for zero and negative zero */
        if (xa == 0.0) {
            long bits = Double.doubleToRawLongBits(x);
            if (bits < 0) {
                return -0.0;
            }
            return 0.0;
        }

        if (xa != xa || xa == Double.POSITIVE_INFINITY) {
            return Double.NaN;
        }

        /* Perform any argument reduction */
        if (xa > 3294198.0) {
            // PI * (2**20)
            // Argument too big for CodyWaite reduction.  Must use
            // PayneHanek.
            double reduceResults[] = new double[3];
            reducePayneHanek(xa, reduceResults);
            quadrant = ((int) reduceResults[0]) & 3;
            xa = reduceResults[1];
            xb = reduceResults[2];
        } else if (xa > 1.5707963267948966) {
            final CodyWaite cw = new CodyWaite(xa);
            quadrant = cw.getK() & 3;
            xa = cw.getRemA();
            xb = cw.getRemB();
        }

        if (negative) {
            quadrant ^= 2;  // Flip bit 1
        }

        switch (quadrant) {
            case 0:
                return sinQ(xa, xb);
            case 1:
                return cosQ(xa, xb);
            case 2:
                return -sinQ(xa, xb);
            case 3:
                return -cosQ(xa, xb);
            default:
                return Double.NaN;
        }
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 100000;

        // Hot loop: force JIT compile of sin/sinQ/cosQ/polySine/polyCosine.
        long fails = 0;
        long total = 0;
        for (int it = 0; it < iters; it++) {
            for (int k = 1; k < 64; k++) {
                double x = k * (Math.PI / 32.0);   // covers (0, 2*pi)
                double got = sin(x);
                double want = Math.sin(x);
                total++;
                if (Math.abs(got - want) > 1e-12) {
                    fails++;
                }
            }
        }
        System.out.println("total=" + total + " fails=" + fails);

        // Detailed pass (everything now JIT'd): print first mismatches.
        int printed = 0;
        for (int k = 1; k < 64 && printed < 10; k++) {
            double x = k * (Math.PI / 32.0);
            double got = sin(x);
            double want = Math.sin(x);
            if (Math.abs(got - want) > 1e-12) {
                System.out.println("MISMATCH k=" + k + " x=" + x + " got=" + got + " want=" + want + " ratio=" + (got / want));
                printed++;
            }
        }

        // Direct probes of the building blocks.
        double e = 0.0353981633974483;
        System.out.println("sinQ(pi/4,0)       = " + sinQ(Math.PI / 4, 0.0) + "  want " + Math.sin(Math.PI / 4));
        System.out.println("sin(pi/4)          = " + sin(Math.PI / 4) + "  want " + Math.sin(Math.PI / 4));
        System.out.println("sin(3pi/4)         = " + sin(3 * Math.PI / 4) + "  want " + Math.sin(3 * Math.PI / 4));
        System.out.println("cosQ(pi/4,0)       = " + cosQ(Math.PI / 4, 0.0) + "  want " + Math.cos(Math.PI / 4));
        System.out.println("polySine(eps)      = " + polySine(e) + "  want " + (Math.sin(e) - e));
        System.out.println("polyCosine(eps)    = " + polyCosine(e) + "  want " + (Math.cos(e) - 1.0));
    }
}
