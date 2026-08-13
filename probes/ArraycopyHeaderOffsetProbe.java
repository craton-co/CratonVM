import java.util.Arrays;

/**
 * The JIT's `System.arraycopy` intrinsic reads the object header at raw byte
 * offsets 4 (kind) and 5 (element_type). Those were the offsets before the
 * header shrank 24 -> 16 on 2026-08-07 and the kind/element_type quartet moved
 * into the mark word; byte 4 is now the low byte of `shape` — an ARRAY'S
 * LENGTH — and byte 5 is the next length byte.
 *
 * What that predicts, if the intrinsic is still emitting those reads:
 *
 *   "is an array"        := (length         & 0xFF) == 1
 *   "element type"       := (length >>> 8)  & 0xFF
 *   "primitive"          := that byte >= 4
 *   width shift          := ((that byte) - 4) & 3
 *
 * So an array whose length is 0x0401 (1025) reports "array" and
 * "element_type 4", and 4 means shift 0 — a ONE-BYTE element. Copying a
 * `long[1025]` would then move 1025 bytes instead of 8200, leaving most of the
 * destination untouched. A `long[256]` (0x0100) fails the first test and takes
 * the correct fallback, which is why this needs a specific length to show.
 *
 * Lengths are chosen for that: 1025 = 0x0401 trips it, 1024 = 0x0400 and
 * 300 = 0x012C do not. Every array is filled with a distinctive pattern and
 * compared element-by-element after the copy, so a short copy shows up as a
 * mismatch index rather than a crash.
 */
public final class ArraycopyHeaderOffsetProbe {

	public static void main(String[] args) {
		// Warm the call site so the JIT compiles it and the intrinsic (if any)
		// is what runs for the measured copies.
		for (int i = 0; i < 200_000; i++) {
			warm();
		}
		check("long[1025]  (0x0401 - predicted to trip)", 1025);
		check("long[1024]  (0x0400 - control)", 1024);
		check("long[300]   (0x012C - control)", 300);
		check("long[257]   (0x0101 - low byte 1, elem byte 1)", 257);
		checkInt("int[1025]   (0x0401 - predicted to trip)", 1025);
		widths();
		refusals();
	}

	/**
	 * Every primitive width at a length that trips the old guard, so a wrong
	 * element-width shift shows up as a short copy rather than passing by luck.
	 */
	private static void widths() {
		int n = 1025;
		byte[] b = new byte[n], b2 = new byte[n];
		short[] s = new short[n], s2 = new short[n];
		char[] c = new char[n], c2 = new char[n];
		double[] d = new double[n], d2 = new double[n];
		for (int i = 0; i < n; i++) {
			b[i] = (byte) (i | 1);
			s[i] = (short) (0x4100 | (i & 0xFF));
			c[i] = (char) (0x5100 | (i & 0xFF));
			d[i] = i + 0.5d;
		}
		System.arraycopy(b, 0, b2, 0, n);
		System.arraycopy(s, 0, s2, 0, n);
		System.arraycopy(c, 0, c2, 0, n);
		System.arraycopy(d, 0, d2, 0, n);
		System.out.println("byte[1025]   -> " + (Arrays.equals(b, b2) ? "OK" : "MISMATCH"));
		System.out.println("short[1025]  -> " + (Arrays.equals(s, s2) ? "OK" : "MISMATCH"));
		System.out.println("char[1025]   -> " + (Arrays.equals(c, c2) ? "OK" : "MISMATCH"));
		System.out.println("double[1025] -> " + (Arrays.equals(d, d2) ? "OK" : "MISMATCH"));
	}

	/**
	 * The guards must still REFUSE what they always refused. A fast path that
	 * merely stopped truncating but now accepts a reference array, a mismatched
	 * pair or an out-of-range position would be a worse bug than the one fixed.
	 */
	private static void refusals() {
		Object[] refSrc = new String[1025];
		Object[] refDst = new Object[1025];
		Arrays.fill(refSrc, "x");
		System.arraycopy(refSrc, 0, refDst, 0, 1025);
		System.out.println("reference[1025] copy -> " + ("x".equals(refDst[1024]) ? "OK" : "MISMATCH"));

		expect("mismatched element kinds", ArrayStoreException.class,
				() -> System.arraycopy(new long[1025], 0, new int[1025], 0, 1025));
		expect("srcPos out of range", IndexOutOfBoundsException.class,
				() -> System.arraycopy(new long[1025], 1, new long[1025], 0, 1025));
		expect("negative length", IndexOutOfBoundsException.class,
				() -> System.arraycopy(new long[1025], 0, new long[1025], 0, -1));
		expect("null src", NullPointerException.class,
				() -> System.arraycopy(null, 0, new long[1025], 0, 1));
	}

	private static void expect(String what, Class<? extends Throwable> expected, Runnable body) {
		try {
			body.run();
			System.out.println(what + " -> NO EXCEPTION (expected " + expected.getSimpleName() + ")");
		}
		catch (Throwable ex) {
			System.out.println(what + " -> " + ex.getClass().getSimpleName()
					+ (expected.isInstance(ex) ? " (as expected)" : " (EXPECTED " + expected.getSimpleName() + ")"));
		}
	}

	private static void warm() {
		long[] a = new long[8];
		long[] b = new long[8];
		Arrays.fill(a, 7L);
		System.arraycopy(a, 0, b, 0, 8);
		if (b[7] != 7L) {
			throw new AssertionError("warm-up copy failed");
		}
	}

	private static void check(String label, int len) {
		long[] src = new long[len];
		long[] dst = new long[len];
		for (int i = 0; i < len; i++) {
			src[i] = 0x0102030405060700L | (i & 0xFF);
		}
		System.arraycopy(src, 0, dst, 0, len);
		int firstBad = -1;
		for (int i = 0; i < len; i++) {
			if (dst[i] != src[i]) {
				firstBad = i;
				break;
			}
		}
		System.out.println(label + " -> " + (firstBad < 0 ? "OK, all " + len + " elements copied"
				: "MISMATCH at index " + firstBad + " (dst=" + dst[firstBad] + " src=" + src[firstBad]
						+ "); copied bytes look like " + (firstBad * 8L) + " of " + (len * 8L)));
	}

	private static void checkInt(String label, int len) {
		int[] src = new int[len];
		int[] dst = new int[len];
		for (int i = 0; i < len; i++) {
			src[i] = 0x11223300 | (i & 0xFF);
		}
		System.arraycopy(src, 0, dst, 0, len);
		int firstBad = -1;
		for (int i = 0; i < len; i++) {
			if (dst[i] != src[i]) {
				firstBad = i;
				break;
			}
		}
		System.out.println(label + " -> " + (firstBad < 0 ? "OK, all " + len + " elements copied"
				: "MISMATCH at index " + firstBad));
	}

}
