import java.nio.charset.StandardCharsets;

public class YamlGrow {
    static final String LINE = "- some list entry\n";

    public static void main(String[] args) throws Exception {
        int target = args.length > 0 ? Integer.parseInt(args[0]) : 4_194_304;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 3;
        String variant = args.length > 2 ? args[2] : "plain";

        for (int rep = 0; rep < reps; rep++) {
            String s = variant.equals("cat2") ? buildCat2(target) : build(target);
            int bad = verifyChars(s);
            if (bad >= 0) {
                System.out.println("PROBE-FAIL rep=" + rep + " stage=chars " + describe(s, bad));
                return;
            }
            byte[] b = s.getBytes(StandardCharsets.UTF_8);
            if (b.length != s.length()) {
                System.out.println("PROBE-FAIL rep=" + rep + " stage=utf8-length chars="
                        + s.length() + " bytes=" + b.length);
                return;
            }
            int badb = verifyBytes(b);
            if (badb >= 0) {
                System.out.println("PROBE-FAIL rep=" + rep + " stage=bytes " + describeBytes(b, badb));
                return;
            }
            System.out.println("rep " + rep + " ok len=" + s.length());
        }
        System.out.println("PROBE-OK " + reps + " reps target=" + target + " variant=" + variant);
    }

    /** Verbatim shape of OriginTrackedYamlLoaderTests.canLoadFilesBiggerThan3Mb. */
    static String build(int target) {
        StringBuilder yaml = new StringBuilder();
        while (yaml.length() < target) {
            yaml.append(LINE);
        }
        return yaml.toString();
    }

    /**
     * Same loop with a cat-2 local whose slot is reused by a cat-1 local in a
     * disjoint range - the exact shape behind the Arrays.sort OSR miscompile.
     */
    static String buildCat2(int target) {
        StringBuilder yaml = new StringBuilder();
        long checksum = 0;
        while (yaml.length() < target) {
            yaml.append(LINE);
            checksum += yaml.length();
        }
        int i = (int) (checksum & 0xff);
        if (i == -1) {
            yaml.append('x');
        }
        return yaml.toString();
    }

    static int verifyChars(String s) {
        int n = LINE.length();
        if (s.length() % n != 0) return s.length() - (s.length() % n);
        for (int i = 0; i < s.length(); i++) {
            if (s.charAt(i) != LINE.charAt(i % n)) return i;
        }
        return -1;
    }

    static int verifyBytes(byte[] b) {
        int n = LINE.length();
        for (int i = 0; i < b.length; i++) {
            if (b[i] != (byte) LINE.charAt(i % n)) return i;
        }
        return -1;
    }

    static String describe(String s, int bad) {
        int from = Math.max(0, bad - 40), to = Math.min(s.length(), bad + 40);
        return "firstBadChar=" + bad + " line=" + (bad / LINE.length())
                + " ctx=[" + s.substring(from, to).replace("\n", "\\n") + "]"
                + " got=" + (int) s.charAt(bad)
                + " want=" + (int) LINE.charAt(bad % LINE.length())
                + " totalLen=" + s.length();
    }

    static String describeBytes(byte[] b, int bad) {
        StringBuilder sb = new StringBuilder();
        for (int i = Math.max(0, bad - 40); i < Math.min(b.length, bad + 40); i++) {
            sb.append((char) (b[i] & 0xff));
        }
        return "firstBadByte=" + bad + " line=" + (bad / LINE.length())
                + " ctx=[" + sb.toString().replace("\n", "\\n") + "] total=" + b.length;
    }
}
