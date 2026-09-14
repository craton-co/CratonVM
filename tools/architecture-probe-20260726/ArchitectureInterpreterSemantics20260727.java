public final class ArchitectureInterpreterSemantics20260727 {
    interface Op {
        int apply(int value);
    }

    static final class Add implements Op {
        private final int delta;

        Add(int delta) {
            this.delta = delta;
        }

        @Override
        public int apply(int value) {
            return value + delta;
        }

        @Override
        public int hashCode() {
            return 0x5a17 + delta;
        }
    }

    private static int staticMix(int a, int b) {
        return (a * 31 + b) ^ (a >>> 3);
    }

    private long instanceMix(long a, long b) {
        return (a + b) ^ (a << 7) ^ (b >>> 11);
    }

    private static long kernel(int iterations) {
        int[] ints = {1, -2, 3, -4, 5, -6, 7, -8};
        long[] longs = {
            0xfffc000000000001L,
            Long.MIN_VALUE,
            0x7fff0000deadbeefL,
            -17L
        };
        float[] floats = {0.0f, -0.0f, 1.25f, Float.intBitsToFloat(0x7fc01234)};
        double[] doubles = {0.0d, -0.0d, Math.PI, Double.longBitsToDouble(0x7ff8000000001234L)};
        Object[] refs = new Object[4];
        refs[0] = "craton";
        refs[1] = Integer.valueOf(17);
        refs[2] = null;
        refs[3] = new Add(9);

        ArchitectureInterpreterSemantics20260727 self =
            new ArchitectureInterpreterSemantics20260727();
        Op op = (Op) refs[3];
        long hash = 0x123456789abcdef0L;
        int local = 3;
        for (int i = 0; i < iterations; i++) {
            int slot = i & 7;
            int value = ints[slot];
            value = staticMix(value + local, i);
            if ((i & 1) == 0) {
                value -= op.apply(i & 31);
            } else if (value < 0) {
                value = -value;
            } else {
                value ^= 0x55aa55aa;
            }
            ints[slot] = value;
            local += (value & 7) - 3;

            int wide = i & 3;
            long lv = longs[wide];
            lv = self.instanceMix(lv, ((long) value << 32) ^ i);
            longs[wide] = lv;

            float fv = floats[wide];
            fv = (fv + value) * 0.5f;
            floats[wide] = fv;

            double dv = doubles[wide];
            dv = (dv - lv) / (wide + 1.0d);
            doubles[wide] = dv;

            Object ref = refs[slot & 3];
            hash ^= ref == null ? 0x9e3779b97f4a7c15L : ref.hashCode();
            hash = Long.rotateLeft(hash + lv + value, 9);
        }

        try {
            hash ^= 17 / (iterations - iterations);
        } catch (ArithmeticException expected) {
            hash ^= 0x6a09e667f3bcc909L;
        }
        try {
            Object[] strings = new String[1];
            strings[0] = Integer.valueOf(1);
            hash ^= 1;
        } catch (ArrayStoreException expected) {
            hash ^= 0xbb67ae8584caa73bL;
        }

        for (int value : ints) {
            hash = Long.rotateLeft(hash ^ value, 5);
        }
        for (long value : longs) {
            hash = Long.rotateLeft(hash ^ value, 7);
        }
        for (float value : floats) {
            hash = Long.rotateLeft(hash ^ Float.floatToRawIntBits(value), 11);
        }
        for (double value : doubles) {
            hash = Long.rotateLeft(hash ^ Double.doubleToRawLongBits(value), 13);
        }
        return hash;
    }

    public static void main(String[] args) {
        int iterations = args.length == 0 ? 20_000 : Integer.parseInt(args[0]);
        System.out.println(Long.toUnsignedString(kernel(iterations)));
    }
}
