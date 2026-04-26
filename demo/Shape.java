public abstract class Shape {
    private String name;

    public Shape(String name) {
        this.name = name;
    }

    public String getName() { return name; }

    public abstract double area();
    public abstract double perimeter();

    public String toString() {
        return name + " [area=" + String.format("%.2f", area())
             + ", perimeter=" + String.format("%.2f", perimeter()) + "]";
    }
}
