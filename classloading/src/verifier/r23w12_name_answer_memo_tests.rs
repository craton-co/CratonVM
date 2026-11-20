use super::*;
use std::cell::Cell;

#[derive(Default)]
struct Counting {
    interface: Cell<u32>,
    resolvable: Cell<u32>,
    direct: Cell<u32>,
    throwable: Cell<u32>,
}

impl ClassHierarchy for Counting {
    fn is_subclass(&self, _child: &str, _parent: &str) -> bool {
        false
    }
    fn common_superclass(&self, _a: &str, _b: &str) -> String {
        "java/lang/Object".to_string()
    }
    fn is_interface(&self, name: &str) -> bool {
        self.interface.set(self.interface.get() + 1);
        name == "p/I"
    }
    fn is_resolvable(&self, name: &str) -> bool {
        self.resolvable.set(self.resolvable.get() + 1);
        name != "p/Missing"
    }
    fn is_direct_superclass(&self, child: &str, parent: &str) -> bool {
        self.direct.set(self.direct.get() + 1);
        child == "p/B" && parent == "p/A"
    }
    fn catch_type_is_throwable(&self, name: &str) -> Option<bool> {
        self.throwable.set(self.throwable.get() + 1);
        match name {
            "p/E" => Some(true),
            "p/A" => Some(false),
            _ => None,
        }
    }
}

fn ask(w: &UndecidedIsAssignable<'_>) -> Vec<String> {
    vec![
        format!("{}", w.is_interface("p/I")),
        format!("{}", w.is_interface("p/A")),
        format!("{}", w.is_resolvable("p/Missing")),
        format!("{}", w.is_resolvable("p/A")),
        format!("{}", w.is_direct_superclass("p/B", "p/A")),
        format!("{}", w.is_direct_superclass("p/A", "p/B")),
        // undecided: `[Lp/Missing;` names an unloaded element.
        format!("{}", w.is_direct_superclass("p/A", "[Lp/Missing;")),
        format!("{:?}", w.catch_type_is_throwable("p/E")),
        format!("{:?}", w.catch_type_is_throwable("p/A")),
        format!("{:?}", w.catch_type_is_throwable("p/Z")),
    ]
}

#[test]
fn the_walk_asks_each_name_once_and_answers_as_before() {
    const KEY: &str = "CRATONVM_VERIFY_NAME_ANSWER_MEMO";
    let (on_answers, on_counts) = cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
        let inner = Counting::default();
        let w = UndecidedIsAssignable::new(&inner, false);
        let first = ask(&w);
        let second = ask(&w);
        assert_eq!(first, second, "a memoised answer is the computed one");
        (
            first,
            (inner.interface.get(), inner.resolvable.get(), inner.direct.get(), inner.throwable.get()),
        )
    });
    let (off_answers, off_counts) =
        cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            let inner = Counting::default();
            let w = UndecidedIsAssignable::new(&inner, false);
            let first = ask(&w);
            let _ = ask(&w);
            (
                first,
                (inner.interface.get(), inner.resolvable.get(), inner.direct.get(), inner.throwable.get()),
            )
        });
    assert_eq!(on_answers, off_answers, "the memo changes no answer");
    assert_eq!(
        on_answers,
        vec!["true", "false", "false", "true", "true", "false", "true", "Some(true)", "Some(false)", "None"]
    );
    // On: two interface names, three direct pairs, three catch types,
    // each asked once; `is_resolvable` once per distinct name
    // (`p/Missing`, `p/A`, `p/B`).
    assert_eq!(on_counts, (2, 3, 3, 3));
    // Off: every question reaches the hierarchy, twice over.
    assert!(off_counts.0 == 4 && off_counts.2 == 6 && off_counts.3 == 6, "{off_counts:?}");
}
