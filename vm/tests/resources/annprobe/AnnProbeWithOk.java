// Same annotation, but the Class value is resolvable by ANY loader (no filter).
// Loaded via the filtering loader, value() must RETURN the class (no false TNPE).
@AnnProbeExampleAnnotation(String.class)
public class AnnProbeWithOk {
}
