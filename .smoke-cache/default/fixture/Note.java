import jakarta.persistence.*;
@Entity
public class Note {
    @Id @GeneratedValue public Long id;
    @Column public String text;
}
