    use super::*;

    #[test]
    fn map_answers_fold_across_the_mapped_pcs() {
        assert_eq!(MapAnswer::from_rows([].into_iter()), MapAnswer::Unknown);
        assert_eq!(
            MapAnswer::from_rows([false, true].into_iter()),
            MapAnswer::Reference
        );
        assert_eq!(
            MapAnswer::from_rows([false, false].into_iter()),
            MapAnswer::NotReference
        );
    }

    #[test]
    fn classify_names_each_disagreement() {
        use MapAnswer::*;
        // A live, non-null reference the scan did not root.
        assert_eq!(classify(false, true, Reference, true), Verdict::Missed);
        // Null or dead references are not owed a root.
        assert_eq!(classify(false, false, Reference, true), Verdict::Agree);
        assert_eq!(classify(false, true, Reference, false), Verdict::Agree);
        // A rooted primitive / `Top` slot.
        assert_eq!(classify(true, true, NotReference, true), Verdict::Extra);
        // Agreement both ways, and no map.
        assert_eq!(classify(true, true, Reference, true), Verdict::Agree);
        assert_eq!(classify(false, true, NotReference, true), Verdict::Agree);
        assert_eq!(classify(true, true, Unknown, true), Verdict::Unknown);
    }
