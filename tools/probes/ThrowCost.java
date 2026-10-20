/**
 * Isolates ordinary Throwable construction from VM-minted implicit exceptions.
 *
 * Compile once, then run each VM in a fresh process. The printed sink makes
 * every constructed throwable observable and also detects a lost message.
 */
public final class ThrowCost {
    static final int[] A = new int[4];
    static int sink;

    static final class NoTraceException extends Exception {
        NoTraceException(String message) {
            super(message, null, false, false);
        }
    }

    static int messageLength(Throwable e) {
        String message = e.getMessage();
        return message == null ? 0 : message.length();
    }

    static int vmMinted(int iterations) {
        int sum = 0;
        for (int i = 0; i < iterations; i++) {
            try {
                sum += A[i + 8];
            } catch (ArrayIndexOutOfBoundsException e) {
                sum += messageLength(e);
            }
        }
        return sum;
    }

    static int javaNewThrown(int iterations) {
        int sum = 0;
        for (int i = 0; i < iterations; i++) {
            try {
                throw new ArrayIndexOutOfBoundsException("Index 9 out of bounds for length 4");
            } catch (ArrayIndexOutOfBoundsException e) {
                sum += messageLength(e);
            }
        }
        return sum;
    }

    static int javaNewOnly(int iterations) {
        int sum = 0;
        for (int i = 0; i < iterations; i++) {
            ArrayIndexOutOfBoundsException e =
                    new ArrayIndexOutOfBoundsException("Index 9 out of bounds for length 4");
            sum += messageLength(e);
        }
        return sum;
    }

    static int allocationOnly(int iterations) {
        int sum = 0;
        for (int i = 0; i < iterations; i++) {
            Object object = new Object();
            sum += object.hashCode() & 1;
        }
        return sum;
    }

    static int javaNewNoTrace(int iterations) {
        int sum = 0;
        for (int i = 0; i < iterations; i++) {
            NoTraceException e = new NoTraceException("Index 9 out of bounds for length 4");
            sum += messageLength(e);
        }
        return sum;
    }

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        for (int round = 0; round < rounds; round++) {
            long start = System.nanoTime();
            sink += vmMinted(iterations);
            long minted = System.nanoTime();
            sink += javaNewThrown(iterations);
            long thrown = System.nanoTime();
            sink += javaNewOnly(iterations);
            long constructed = System.nanoTime();
            sink += allocationOnly(iterations);
            long allocated = System.nanoTime();
            sink += javaNewNoTrace(iterations);
            long noTrace = System.nanoTime();
            System.out.println(
                    "round " + round
                            + " vmMinted " + ((minted - start) / iterations)
                            + " javaNewThrown " + ((thrown - minted) / iterations)
                            + " javaNewOnly " + ((constructed - thrown) / iterations)
                            + " allocOnly " + ((allocated - constructed) / iterations)
                            + " javaNewNoTrace " + ((noTrace - allocated) / iterations));
        }
        System.out.println("sink " + sink);
    }
}
