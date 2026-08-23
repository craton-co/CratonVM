public class StripProbe {
    static void show(String tag, String s) {
        System.out.printf("%s len=%d [%s]%n", tag, s.length(), s);
    }
    public static void main(String[] args) {
        String s = "Why is the sky blue?";
        show("RAW   ", s);
        show("STRIP ", s.strip());
        show("TRIM  ", s.trim());
        show("SLEAD ", s.stripLeading());
        show("STRAIL", s.stripTrailing());
        show("PAD   ", "  padded text here  ".strip());
        System.out.printf("ISBLANK %b EMPTY %b%n", s.isBlank(), s.isEmpty());
    }
}
