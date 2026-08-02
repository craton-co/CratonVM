// Regression probe: `Enum.name()` / `Enum.toString()` must answer the JVM
// CONSTANT name, even when the enum declares its own field called `name`.
//
// Measured 2026-08-01 on dev before the fix:
//
//     Enum.name()     = "Jersey"   want "JERSEY"
//     Enum.toString() = "Jersey"   want "JERSEY"
//     Enum.valueOf(..., "JERSEY")  -> IllegalArgumentException: No enum constant JERSEY
//
// Downstream that killed every JUnit `@TestTemplate` class whose invocation
// contexts are keyed on such an enum: the enum-valued annotation attribute
// resolved to nothing, so JUnit refused the descriptor with
// `PreconditionViolationException: displayName must not be null or blank`, and
// `IntegrationGraphEndpointWebIntegrationTests` reported `containersFailed=2,
// tests=0` in 1.2 s, before any Spring context started.
//
// `java.lang.Enum` declares `private final String name`. An enum that declares
// its OWN field also called `name` — which Spring Boot's
// `WebEndpointTest.Infrastructure` does, and which supplies the JUnit
// @TestTemplate display name — has TWO fields called `name` in one object.
//
// `native_enum_name` (Enum.name() AND Enum.toString()) resolves "name" by name,
// and `resolve_field_index_by_class_id` returns the MOST-DERIVED declaration,
// so the two can disagree about which slot they mean. `field_read::ref_field`
// degrades any non-Object tag to null, so an unwritten slot surfaces as null
// rather than loudly.
//
// Prints what each reader actually answers instead of asserting a guess.
public class EnumShadowedNameProbe {

    enum Shadowed {
        JERSEY("Jersey"),
        MVC("WebMvc"),
        WEBFLUX("WebFlux");

        private final String name;   // shadows java.lang.Enum.name

        Shadowed(String name) {
            this.name = name;
        }

        String appName() {
            return this.name;
        }
    }

    enum Plain {
        ALPHA("A"),
        BETA("B");

        private final String label;  // no shadowing

        Plain(String label) {
            this.label = label;
        }

        String label() {
            return this.label;
        }
    }

    static int bad = 0;

    static void show(String what, String got, String want) {
        boolean ok = want.equals(got);
        if (!ok) {
            bad++;
        }
        System.out.println((ok ? "  ok   " : "  BAD  ") + what
                + " = " + q(got) + (ok ? "" : "   want " + q(want)));
    }

    public static void main(String[] args) throws Exception {
        System.out.println("-- enum whose own field shadows Enum.name --");
        for (Shadowed s : Shadowed.values()) {
            String constant = switch (s.ordinal()) {
                case 0 -> "JERSEY";
                case 1 -> "MVC";
                default -> "WEBFLUX";
            };
            String app = switch (s.ordinal()) {
                case 0 -> "Jersey";
                case 1 -> "WebMvc";
                default -> "WebFlux";
            };
            show("Enum.name()   ", s.name(), constant);
            show("Enum.toString()", s.toString(), constant);
            show("appName()     ", s.appName(), app);
            show("valueOf.name()", Shadowed.valueOf(constant).name(), constant);
            // Reflection is how JUnit/Spring often reach it
            java.lang.reflect.Field f = Shadowed.class.getDeclaredField("name");
            f.setAccessible(true);
            show("reflect name  ", String.valueOf(f.get(s)), app);
        }

        System.out.println("-- control: enum with a non-shadowing field --");
        for (Plain p : Plain.values()) {
            String constant = p.ordinal() == 0 ? "ALPHA" : "BETA";
            String label = p.ordinal() == 0 ? "A" : "B";
            show("Enum.name()   ", p.name(), constant);
            show("label()       ", p.label(), label);
        }

        System.out.println("EnumShadowedNameProbe bad=" + bad);
        System.exit(bad == 0 ? 0 : 1);
    }

    static String q(Object o) {
        return o == null ? "null" : "\"" + o + "\"";
    }
}
