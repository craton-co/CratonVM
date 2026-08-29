import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.AccessController;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.security.PrivilegedAction;
import java.security.PrivilegedActionException;
import java.security.PrivilegedExceptionAction;

/** L8 tail — `java.security`: `MessageDigest` (17 rows), `AccessController` (4),
 *  and the small `CodeSource` / `Permission` / `ProtectionDomain` surface.
 *
 *  A digest is the friendliest thing in this campaign to compare: the output is
 *  a pure function of the input bytes, fixed by a published standard, and every
 *  algorithm here has test vectors older than either VM. So the sweep is over
 *  ALGORITHMS x INPUTS x FEEDING PATTERNS, and the last of those three is the
 *  point — `update(byte)`, `update(byte[])`, `update(byte[],int,int)` and
 *  `update(ByteBuffer)` must all reach the same digest for the same bytes, and
 *  an implementation that buffers wrongly fails only for one of them and only
 *  at particular lengths. The lengths chosen straddle every block boundary the
 *  algorithms use (55/56/63/64 for the 512-bit block, 111/112/127/128 for the
 *  1024-bit one) because that is where a padding bug lives.
 *
 *  WHAT IS NOT ASKED: the provider's NAME. `getProvider()` names whoever
 *  implements the algorithm and CratonVM is entitled to answer differently from
 *  `SUN`; what is asked instead is that a provider is present, that its name is
 *  stable across calls, and that the digest is right — which is the property a
 *  caller actually depends on.
 */
public class SecuritySurfaceSweep {

    static int rows = 0;

    interface F {
        Object get() throws Throwable;
    }

    static final char[] HEX = "0123456789abcdef".toCharArray();

    static void p(String tag, F f) {
        rows++;
        String v;
        try {
            v = String.valueOf(f.get());
        } catch (Throwable e) {
            v = "THREW " + e.getClass().getName() + ": " + e.getMessage();
        }
        System.out.println(tag + " |" + v + "|");
    }

    static void sect(String name, Runnable r) {
        try {
            r.run();
        } catch (Throwable e) {
            System.out.println("SECTION-ABORTED " + name + " " + e.getClass().getName());
        }
    }

    static String hex(byte[] b) {
        if (b == null) {
            return "null";
        }
        char[] out = new char[b.length * 2];
        for (int i = 0; i < b.length; i++) {
            out[i * 2] = HEX[(b[i] >> 4) & 0xf];
            out[i * 2 + 1] = HEX[b[i] & 0xf];
        }
        return new String(out);
    }

    static final String[] ALGS = {
        "MD5", "SHA-1", "SHA-224", "SHA-256", "SHA-384", "SHA-512",
        "SHA3-224", "SHA3-256", "SHA3-384", "SHA3-512",
    };

    /** Lengths that straddle every padding boundary the two block sizes use. */
    static final int[] LENGTHS = {
        0, 1, 2, 3, 55, 56, 57, 63, 64, 65, 111, 112, 113, 127, 128, 129, 255, 256, 1000,
    };

    static byte[] input(int n) {
        byte[] b = new byte[n];
        for (int i = 0; i < n; i++) {
            b[i] = (byte) (i * 31 + 7);
        }
        return b;
    }

    // ------------------------------------------------------ 1. the digests

    static void digests() {
        for (String alg : ALGS) {
            for (int n : LENGTHS) {
                byte[] in = input(n);
                String t = "[" + alg + " " + n + "]";
                p(t + " digest(byte[])", () -> hex(MessageDigest.getInstance(alg).digest(in)));
                // Every feeding pattern must reach the same digest. An
                // implementation that buffers wrongly fails for exactly one of
                // these, at exactly one length class.
                p(t + " update(byte[]) then digest()", () -> {
                    MessageDigest md = MessageDigest.getInstance(alg);
                    md.update(in);
                    return hex(md.digest());
                });
                p(t + " byte-at-a-time", () -> {
                    MessageDigest md = MessageDigest.getInstance(alg);
                    for (byte b : in) {
                        md.update(b);
                    }
                    return hex(md.digest());
                });
                p(t + " split in three", () -> {
                    MessageDigest md = MessageDigest.getInstance(alg);
                    int a = in.length / 3;
                    int b = in.length / 2;
                    md.update(in, 0, a);
                    md.update(in, a, b - a);
                    md.update(in, b, in.length - b);
                    return hex(md.digest());
                });
                p(t + " ByteBuffer", () -> {
                    MessageDigest md = MessageDigest.getInstance(alg);
                    md.update(ByteBuffer.wrap(in));
                    return hex(md.digest());
                });
                p(t + " direct ByteBuffer", () -> {
                    MessageDigest md = MessageDigest.getInstance(alg);
                    ByteBuffer bb = ByteBuffer.allocateDirect(in.length);
                    bb.put(in);
                    bb.flip();
                    md.update(bb);
                    return hex(md.digest());
                });
            }
        }
    }

    // ----------------------------------------------------- 2. the lifecycle

    static void lifecycle() {
        for (String alg : ALGS) {
            String t = "[" + alg + "]";
            p(t + " getAlgorithm", () -> MessageDigest.getInstance(alg).getAlgorithm());
            p(t + " getDigestLength", () -> MessageDigest.getInstance(alg).getDigestLength());
            p(t + " digest length matches getDigestLength", () -> {
                MessageDigest md = MessageDigest.getInstance(alg);
                return md.digest().length == md.getDigestLength();
            });
            p(t + " provider is present", () -> MessageDigest.getInstance(alg).getProvider() != null);
            p(t + " provider name is stable", () -> {
                String a = MessageDigest.getInstance(alg).getProvider().getName();
                String b = MessageDigest.getInstance(alg).getProvider().getName();
                return a.equals(b);
            });
            p(t + " two instances are distinct", () -> {
                return MessageDigest.getInstance(alg) != MessageDigest.getInstance(alg);
            });
            p(t + " reset then digest is the empty digest", () -> {
                MessageDigest md = MessageDigest.getInstance(alg);
                md.update(input(100));
                md.reset();
                String after = hex(md.digest());
                return after.equals(hex(MessageDigest.getInstance(alg).digest()));
            });
            p(t + " digest() resets implicitly", () -> {
                MessageDigest md = MessageDigest.getInstance(alg);
                md.update(input(50));
                md.digest();
                return hex(md.digest()).equals(hex(MessageDigest.getInstance(alg).digest()));
            });
            p(t + " digest(byte[]) after update", () -> {
                MessageDigest md = MessageDigest.getInstance(alg);
                md.update(input(10));
                return hex(md.digest(input(20)));
            });
            p(t + " digest into a buffer", () -> {
                MessageDigest md = MessageDigest.getInstance(alg);
                md.update(input(64));
                byte[] out = new byte[md.getDigestLength() + 4];
                int n = md.digest(out, 2, md.getDigestLength());
                return n + ":" + hex(out);
            });
            p(t + " digest into too small a buffer", () -> {
                MessageDigest md = MessageDigest.getInstance(alg);
                byte[] out = new byte[2];
                return md.digest(out, 0, 2);
            });
            p(t + " digest into a null buffer", () -> {
                MessageDigest md = MessageDigest.getInstance(alg);
                return md.digest(null, 0, 4);
            });
            p(t + " toString shape", () -> {
                String s = MessageDigest.getInstance(alg).toString();
                return s != null && s.contains(alg);
            });
            p(t + " clone agrees", () -> {
                MessageDigest md = MessageDigest.getInstance(alg);
                md.update(input(70));
                try {
                    MessageDigest c = (MessageDigest) md.clone();
                    return hex(c.digest()).equals(hex(md.digest()));
                } catch (CloneNotSupportedException e) {
                    return "CloneNotSupportedException";
                }
            });
            p(t + " clone is independent", () -> {
                MessageDigest md = MessageDigest.getInstance(alg);
                md.update(input(70));
                try {
                    MessageDigest c = (MessageDigest) md.clone();
                    c.update(input(5));
                    return !hex(c.digest()).equals(hex(md.digest()));
                } catch (CloneNotSupportedException e) {
                    return "CloneNotSupportedException";
                }
            });
        }
    }

    // ------------------------------------------------------- 3. isEqual

    static void equality() {
        byte[] a = input(32);
        byte[] b = input(32);
        byte[] c = input(31);
        byte[] d = input(32);
        d[31] = (byte) (d[31] ^ 1);
        p("isEqual same content", () -> MessageDigest.isEqual(a, b));
        p("isEqual same array", () -> MessageDigest.isEqual(a, a));
        p("isEqual different length", () -> MessageDigest.isEqual(a, c));
        p("isEqual last byte differs", () -> MessageDigest.isEqual(a, d));
        p("isEqual empty arrays", () -> MessageDigest.isEqual(new byte[0], new byte[0]));
        p("isEqual null and array", () -> MessageDigest.isEqual(null, a));
        p("isEqual array and null", () -> MessageDigest.isEqual(a, null));
        p("isEqual null and null", () -> MessageDigest.isEqual(null, null));
    }

    // ------------------------------------------------------ 4. the refusals

    static void refusals() {
        p("getInstance of an unknown algorithm", () -> {
            try {
                return MessageDigest.getInstance("NO-SUCH-DIGEST").getAlgorithm();
            } catch (NoSuchAlgorithmException e) {
                return e.getClass().getName();
            }
        });
        p("getInstance null", () -> {
            try {
                return MessageDigest.getInstance(null).getAlgorithm();
            } catch (Throwable e) {
                return e.getClass().getName();
            }
        });
        p("getInstance empty", () -> {
            try {
                return MessageDigest.getInstance("").getAlgorithm();
            } catch (Throwable e) {
                return e.getClass().getName();
            }
        });
        p("getInstance with an unknown provider", () -> {
            try {
                return MessageDigest.getInstance("SHA-256", "NoSuchProvider").getAlgorithm();
            } catch (Throwable e) {
                return e.getClass().getName();
            }
        });
        p("getInstance with a null provider name", () -> {
            try {
                return MessageDigest.getInstance("SHA-256", (String) null).getAlgorithm();
            } catch (Throwable e) {
                return e.getClass().getName();
            }
        });
        p("getInstance is case-insensitive", () -> {
            byte[] one = MessageDigest.getInstance("sha-256").digest(input(10));
            byte[] two = MessageDigest.getInstance("SHA-256").digest(input(10));
            return hex(one).equals(hex(two));
        });
        p("SHA-256 alias SHA256", () -> {
            try {
                return MessageDigest.getInstance("SHA256").getAlgorithm();
            } catch (Throwable e) {
                return e.getClass().getName();
            }
        });
        p("update null array", () -> {
            MessageDigest md = MessageDigest.getInstance("SHA-256");
            md.update((byte[]) null);
            return "no throw";
        });
        p("update out of range", () -> {
            MessageDigest md = MessageDigest.getInstance("SHA-256");
            md.update(input(4), 0, 10);
            return "no throw";
        });
        p("update negative offset", () -> {
            MessageDigest md = MessageDigest.getInstance("SHA-256");
            md.update(input(4), -1, 2);
            return "no throw";
        });
        p("update null ByteBuffer", () -> {
            MessageDigest md = MessageDigest.getInstance("SHA-256");
            md.update((ByteBuffer) null);
            return "no throw";
        });
        p("digest of a null array", () -> {
            MessageDigest md = MessageDigest.getInstance("SHA-256");
            return hex(md.digest((byte[]) null));
        });
    }

    // ------------------------------------------------- 5. AccessController

    static void privileged() {
        p("doPrivileged returns the value",
            () -> AccessController.doPrivileged((PrivilegedAction<String>) () -> "value"));
        p("doPrivileged returns null",
            () -> String.valueOf(
                AccessController.doPrivileged((PrivilegedAction<String>) () -> null)));
        p("doPrivileged propagates a RuntimeException", () -> {
            try {
                return AccessController.doPrivileged((PrivilegedAction<String>) () -> {
                    throw new IllegalStateException("inner");
                });
            } catch (Throwable e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("doPrivileged propagates an Error", () -> {
            try {
                return AccessController.doPrivileged((PrivilegedAction<String>) () -> {
                    throw new StackOverflowError("inner");
                });
            } catch (Throwable e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("doPrivileged null action", () -> {
            try {
                return AccessController.doPrivileged((PrivilegedAction<String>) null);
            } catch (Throwable e) {
                return e.getClass().getName();
            }
        });
        p("doPrivileged exception action returns", () -> {
            try {
                return AccessController.doPrivileged(
                    (PrivilegedExceptionAction<String>) () -> "value");
            } catch (Throwable e) {
                return e.getClass().getName();
            }
        });
        // The wrapping is the whole point of the ExceptionAction overload: a
        // CHECKED exception comes back inside PrivilegedActionException, an
        // unchecked one does not.
        p("doPrivileged wraps a checked exception", () -> {
            try {
                AccessController.doPrivileged((PrivilegedExceptionAction<String>) () -> {
                    throw new java.io.IOException("checked");
                });
                return "no throw";
            } catch (PrivilegedActionException e) {
                return e.getClass().getName() + " wrapping "
                        + e.getCause().getClass().getName() + ": " + e.getCause().getMessage();
            } catch (Throwable e) {
                return "OTHER " + e.getClass().getName();
            }
        });
        p("doPrivileged does not wrap an unchecked exception", () -> {
            try {
                AccessController.doPrivileged((PrivilegedExceptionAction<String>) () -> {
                    throw new IllegalStateException("unchecked");
                });
                return "no throw";
            } catch (PrivilegedActionException e) {
                return "WRAPPED " + e.getCause().getClass().getName();
            } catch (Throwable e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("getContext is non-null", () -> AccessController.getContext() != null);
        p("getContext class", () -> AccessController.getContext().getClass().getName());
        p("doPrivileged with a context", () -> AccessController.doPrivileged(
            (PrivilegedAction<String>) () -> "with-context", AccessController.getContext()));
        p("doPrivileged with a null context", () -> AccessController.doPrivileged(
            (PrivilegedAction<String>) () -> "null-context", null));
        p("nested doPrivileged", () -> AccessController.doPrivileged(
            (PrivilegedAction<String>) () -> AccessController.doPrivileged(
                (PrivilegedAction<String>) () -> "nested")));
    }

    // ------------------------------------------------- 6. the small classes

    static void small() {
        p("Permission subclass name", () -> {
            java.security.Permission perm = new java.security.BasicPermission("probe") { };
            return perm.getName();
        });
        p("Permission getActions", () -> {
            java.security.Permission perm = new java.security.BasicPermission("probe") { };
            return String.valueOf(perm.getActions());
        });
        p("Permission toString", () -> {
            java.security.Permission perm = new java.security.AllPermission();
            return perm.toString();
        });
        p("Permission implies itself", () -> {
            java.security.Permission perm = new java.security.AllPermission();
            return perm.implies(perm);
        });
        p("Permission equals", () -> {
            return new java.security.AllPermission().equals(new java.security.AllPermission());
        });
        p("CodeSource with a null location", () -> {
            java.security.CodeSource cs = new java.security.CodeSource(null, (java.security.cert.Certificate[]) null);
            return String.valueOf(cs.getLocation());
        });
        p("CodeSource toString", () -> {
            java.security.CodeSource cs = new java.security.CodeSource(null, (java.security.cert.Certificate[]) null);
            return cs.toString();
        });
        p("CodeSource getCertificates", () -> {
            java.security.CodeSource cs = new java.security.CodeSource(null, (java.security.cert.Certificate[]) null);
            return String.valueOf(cs.getCertificates());
        });
        p("ProtectionDomain toString shape", () -> {
            java.security.ProtectionDomain pd =
                new java.security.ProtectionDomain(null, null);
            return pd.toString() != null && pd.toString().startsWith("ProtectionDomain");
        });
        p("ProtectionDomain implies", () -> {
            java.security.ProtectionDomain pd =
                new java.security.ProtectionDomain(null, null);
            return pd.implies(new java.security.AllPermission());
        });
    }

    public static void main(String[] args) {
        sect("digests", SecuritySurfaceSweep::digests);
        sect("lifecycle", SecuritySurfaceSweep::lifecycle);
        sect("equality", SecuritySurfaceSweep::equality);
        sect("refusals", SecuritySurfaceSweep::refusals);
        sect("privileged", SecuritySurfaceSweep::privileged);
        sect("small", SecuritySurfaceSweep::small);
        System.out.println("rows " + rows);
        System.out.println("DONE SecuritySurfaceSweep");
    }
}
