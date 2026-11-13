import java.util.Random;
import jdk.incubator.vector.*;

public final class VectorEqualityProbe {
    public static void main(String[] args) {
        Random random = new Random(196481);
        long hash = 0;
        var operations = new VectorOperators.Comparison[]{VectorOperators.EQ, VectorOperators.NE,
            VectorOperators.LT, VectorOperators.LE, VectorOperators.GT, VectorOperators.GE};
        for (var species : new VectorSpecies[]{IntVector.SPECIES_128, IntVector.SPECIES_256, IntVector.SPECIES_512}) {
            @SuppressWarnings("unchecked") VectorSpecies<Integer> s = (VectorSpecies<Integer>) species;
            int n = s.length();
            int[] a = new int[n+2], b = new int[n+2];
            for (int step=0; step<256; step++) {
                for (int i=1; i<=n; i++) {
                    a[i] = random.nextInt();
                    b[i] = (i+step)%3 == 0 ? a[i] : random.nextInt();
                }
                a[1] = Integer.MIN_VALUE;
                a[n] = Integer.MAX_VALUE;
                var x = IntVector.fromArray(s, a, 1);
                var y = IntVector.fromArray(s, b, 1);
                var active = VectorMask.fromLong(s, 0xaaaa);
                for (int op=0; op<operations.length; op++) {
                    long expected = 0;
                    for (int i=0; i<n; i++) {
                        int left=a[i+1], right=b[i+1];
                        boolean yes = switch (op) {
                            case 0 -> left == right; case 1 -> left != right;
                            case 2 -> left < right; case 3 -> left <= right;
                            case 4 -> left > right; default -> left >= right;
                        };
                        if (yes) expected |= 1L << i;
                    }
                    var result = x.compare(operations[op], y);
                    var masked = x.compare(operations[op], y, active);
                    if ((step & 63) == 0) System.gc();
                    if (result.toLong() != expected || masked.toLong() != (expected & 0xaaaa))
                        throw new AssertionError("comparison lanes="+n+" op="+op);
                    hash = hash*31+result.toLong();
                    hash = hash*31+masked.toLong();
                }
            }
        }
        float[] a = {Float.NaN,-0f,0f,Float.NEGATIVE_INFINITY,Float.POSITIVE_INFINITY,1,2,3,4,5,6,7,8,9,10,11};
        float[] b = {Float.NaN,0f,-0f,Float.NEGATIVE_INFINITY,Float.POSITIVE_INFINITY,2,2,0,4,5,6,7,8,9,10,11};
        long expected = 0;
        for (int i=0; i<16; i++) if (a[i] == b[i]) expected |= 1L << i;
        if (FloatVector.fromArray(FloatVector.SPECIES_512,a,0).eq(FloatVector.fromArray(FloatVector.SPECIES_512,b,0)).toLong() != expected)
            throw new AssertionError("float fallback");
        System.out.println("VECTOR_EQUALITY_OK " + hash);
    }
}
