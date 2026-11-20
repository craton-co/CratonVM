use super::*;

fn collect(mut it: ShapeIter) -> Vec<[i32; 4]> {
    let mut v = Vec::new();
    while let Some(s) = it.next_span() {
        v.push(s);
    }
    v
}

#[test]
fn integer_rectangle() {
    let mut it = ShapeIter::new(false);
    it.set_rule(1);
    it.move_to(2.0, 1.0);
    it.line_to(6.0, 1.0);
    it.line_to(6.0, 3.0);
    it.line_to(2.0, 3.0);
    it.close_path();
    it.path_done();
    let v = collect(it);
    assert_eq!(v, vec![[2, 1, 6, 2], [2, 2, 6, 3]]);
}

#[test]
fn clip_box_limits_spans() {
    let mut it = ShapeIter::new(false);
    it.set_rule(1);
    it.set_output_area(3, 2, 5, 3);
    it.append_poly(&[2, 6, 6, 2], &[1, 1, 3, 3], 0, 0);
    let v = collect(it);
    assert_eq!(v, vec![[3, 2, 5, 3]]);
}

#[test]
fn even_odd_hole() {
    let mut it = ShapeIter::new(false);
    it.set_rule(0);
    for (x0, y0, x1, y1) in [(0.0f32, 0.0f32, 6.0f32, 6.0f32), (2.0, 2.0, 4.0, 4.0)] {
        it.move_to(x0, y0);
        it.line_to(x1, y0);
        it.line_to(x1, y1);
        it.line_to(x0, y1);
        it.close_path();
    }
    it.path_done();
    let v = collect(it);
    assert!(v.contains(&[0, 2, 2, 3]) && v.contains(&[4, 2, 6, 3]));
    assert!(!v.iter().any(|s| s[1] == 2 && s[0] == 2));
}
