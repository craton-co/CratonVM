import java.util.function.DoubleBinaryOperator;
import java.util.function.DoubleUnaryOperator;

/**
 * HotSpot StrictMath oracle for CratonVM's fdlibm port (types/src/fdlibm.rs).
 *
 * Emits an FNV-1a digest of the RESULT BITS over a deterministic corpus, per
 * function. The Rust side (fdlibm::tests::*_digest_matches_hotspot) builds the
 * identical corpus and the identical digest, so millions of points are covered
 * without committing a vector file.
 *
 * The corpus generator MUST stay identical on both sides. It is:
 *   - a structured prefix: every exponent 0..=0x7ff x 10 mantissas x both signs
 *   - then splitmix64(i) for i in 0.. reinterpreted as f64 bits
 *
 * NaN is canonicalized before hashing: any NaN result hashes as NAN_SENTINEL.
 * StrictMath and the port are both free to choose a NaN payload, and the
 * existing golden tables already treat all NaNs as equal.
 *
 * Usage:
 *   javac FdlibmOracle.java && java FdlibmOracle            # digests
 *   java FdlibmOracle --dump <fn> > fn.txt                  # per-point, to localize a mismatch
 */
public final class FdlibmOracle {
  static final int STRUCTURED_MANTISSAS = 10;
  static final long[] MANTISSAS = {
    0L, 1L, 0x8_0000_0000_0000L, 0xf_ffff_ffff_ffffL,
    0x0_0000_8000_0000L, 0x0_0000_7fff_ffffL,
    0xf_ffff_8000_0000L, 0x7_ffff_7fff_ffffL,
    0x0_0000_c000_0000L, 0x0_0000_4000_0000L,
  };
  static final int RANDOM_POINTS = 200_000;
  static final long NAN_SENTINEL = 0x7ff8_0000_0000_0000L;

  static long splitmix64(long x) {
    x += 0x9E3779B97F4A7C15L;
    long z = x;
    z = (z ^ (z >>> 30)) * 0xBF58476D1CE4E5B9L;
    z = (z ^ (z >>> 27)) * 0x94D049BB133111EBL;
    return z ^ (z >>> 31);
  }

  /** The i-th corpus point, structured prefix first, then splitmix64. */
  static double point(int i) {
    int structured = 0x800 * STRUCTURED_MANTISSAS * 2;
    if (i < structured) {
      int idx = i / 2;
      boolean neg = (i & 1) == 1;
      int exp = idx / STRUCTURED_MANTISSAS;
      long mant = MANTISSAS[idx % STRUCTURED_MANTISSAS];
      long bits = ((long) exp << 52) | mant;
      if (neg) bits |= 0x8000_0000_0000_0000L;
      return Double.longBitsToDouble(bits);
    }
    return Double.longBitsToDouble(splitmix64(i - structured));
  }

  static int corpusSize() { return 0x800 * STRUCTURED_MANTISSAS * 2 + RANDOM_POINTS; }

  static long canon(double d) {
    return Double.isNaN(d) ? NAN_SENTINEL : Double.doubleToRawLongBits(d);
  }

  static long fnv(long h, long v) {
    for (int b = 0; b < 8; b++) {
      h ^= (v >>> (b * 8)) & 0xffL;
      h *= 0x100_0000_01b3L;
    }
    return h;
  }

  static long unary(DoubleUnaryOperator f) {
    long h = 0xcbf29ce484222325L;
    int n = corpusSize();
    for (int i = 0; i < n; i++) h = fnv(h, canon(f.applyAsDouble(point(i))));
    return h;
  }

  /** Binary: stride the corpus so the cross product stays ~4M points. */
  static final int BIN_STRIDE = 97;
  static long binary(DoubleBinaryOperator f) {
    long h = 0xcbf29ce484222325L;
    int n = corpusSize();
    for (int i = 0; i < n; i += BIN_STRIDE)
      for (int j = 0; j < n; j += BIN_STRIDE)
        h = fnv(h, canon(f.applyAsDouble(point(i), point(j))));
    return h;
  }

  /** Per-point dump for one function, so a digest mismatch can be localized. */
  static void dump(String fn) {
    int n = corpusSize();
    DoubleUnaryOperator u = switch (fn) {
      case "log" -> StrictMath::log;       case "sin" -> StrictMath::sin;
      case "cos" -> StrictMath::cos;       case "tan" -> StrictMath::tan;
      case "asin" -> StrictMath::asin;     case "acos" -> StrictMath::acos;
      case "atan" -> StrictMath::atan;     case "cbrt" -> StrictMath::cbrt;
      case "exp" -> StrictMath::exp;       case "log10" -> StrictMath::log10;
      case "log1p" -> StrictMath::log1p;   case "expm1" -> StrictMath::expm1;
      case "sinh" -> StrictMath::sinh;     case "cosh" -> StrictMath::cosh;
      case "tanh" -> StrictMath::tanh;     default -> null;
    };
    if (u != null) {
      for (int i = 0; i < n; i++) {
        double x = point(i);
        System.out.printf("%d %016x %016x%n", i, Double.doubleToRawLongBits(x),
            canon(u.applyAsDouble(x)));
      }
      return;
    }
    DoubleBinaryOperator b = switch (fn) {
      case "atan2" -> StrictMath::atan2;   case "pow" -> StrictMath::pow;
      case "hypot" -> StrictMath::hypot;
      case "IEEEremainder" -> StrictMath::IEEEremainder;
      default -> null;
    };
    if (b == null) {
      System.err.println("unknown function: " + fn);
      System.exit(2);
    }
    for (int i = 0; i < n; i += BIN_STRIDE) {
      for (int j = 0; j < n; j += BIN_STRIDE) {
        double x = point(i), y = point(j);
        System.out.printf("%d %d %016x %016x %016x%n", i, j,
            Double.doubleToRawLongBits(x), Double.doubleToRawLongBits(y),
            canon(b.applyAsDouble(x, y)));
      }
    }
  }

  public static void main(String[] args) {
    if (args.length == 2 && args[0].equals("--dump")) { dump(args[1]); return; }
    System.out.println("corpus_size=" + corpusSize() + " bin_stride=" + BIN_STRIDE);
    System.out.printf("log %#018x%n",     unary(StrictMath::log));
    System.out.printf("sin %#018x%n",     unary(StrictMath::sin));
    System.out.printf("cos %#018x%n",     unary(StrictMath::cos));
    System.out.printf("tan %#018x%n",     unary(StrictMath::tan));
    System.out.printf("asin %#018x%n",    unary(StrictMath::asin));
    System.out.printf("acos %#018x%n",    unary(StrictMath::acos));
    System.out.printf("atan %#018x%n",    unary(StrictMath::atan));
    System.out.printf("cbrt %#018x%n",    unary(StrictMath::cbrt));
    System.out.printf("exp %#018x%n",     unary(StrictMath::exp));
    System.out.printf("log10 %#018x%n",   unary(StrictMath::log10));
    System.out.printf("log1p %#018x%n",   unary(StrictMath::log1p));
    System.out.printf("expm1 %#018x%n",   unary(StrictMath::expm1));
    System.out.printf("sinh %#018x%n",    unary(StrictMath::sinh));
    System.out.printf("cosh %#018x%n",    unary(StrictMath::cosh));
    System.out.printf("tanh %#018x%n",    unary(StrictMath::tanh));
    System.out.printf("atan2 %#018x%n",         binary(StrictMath::atan2));
    System.out.printf("pow %#018x%n",           binary(StrictMath::pow));
    System.out.printf("hypot %#018x%n",         binary(StrictMath::hypot));
    System.out.printf("IEEEremainder %#018x%n", binary(StrictMath::IEEEremainder));
  }
}
