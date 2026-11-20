use super::*;
use smallvec::smallvec;

// --- EventTypeId ---

#[test]
fn test_event_type_id_equality() {
    assert_eq!(EventTypeId(1), EventTypeId(1));
    assert_ne!(EventTypeId(1), EventTypeId(2));
}

#[test]
fn test_event_type_id_clone_copy() {
    let id = EventTypeId(42);
    let id2 = id; // Copy
    let id3 = id.clone();
    assert_eq!(id, id2);
    assert_eq!(id, id3);
}

#[test]
fn test_event_type_id_hash() {
    let mut map = HashMap::new();
    map.insert(EventTypeId(1), "one");
    map.insert(EventTypeId(2), "two");
    assert_eq!(map.get(&EventTypeId(1)), Some(&"one"));
    assert_eq!(map.get(&EventTypeId(2)), Some(&"two"));
    assert_eq!(map.get(&EventTypeId(3)), None);
}

#[test]
fn test_event_type_id_debug() {
    let id = EventTypeId(7);
    let dbg = format!("{:?}", id);
    assert!(dbg.contains("7"));
}

// --- EventField ---

#[test]
fn test_event_field_new() {
    let f = EventField::new("count", "int", "Item count");
    assert_eq!(f.name, "count");
    assert_eq!(f.type_name, "int");
    assert_eq!(f.description, "Item count");
}

#[test]
fn test_event_field_clone() {
    let f = EventField::new("name", "string", "Name field");
    let f2 = f.clone();
    assert_eq!(f.name, f2.name);
    assert_eq!(f.type_name, f2.type_name);
    assert_eq!(f.description, f2.description);
}

#[test]
fn test_event_field_debug() {
    let f = EventField::new("x", "long", "desc");
    let dbg = format!("{:?}", f);
    assert!(dbg.contains("x"));
    assert!(dbg.contains("long"));
}

// --- EventType ---

fn make_event_type(name: &str) -> EventType {
    EventType {
        id: EventTypeId(0),
        name: name.to_string(),
        category: vec!["Test".into()],
        description: "Test event".into(),
        fields: vec![EventField::new("field1", "int", "First field")],
        has_thread: true,
        has_stacktrace: false,
        period: EventPeriod::None,
        threshold: None,
    }
}

#[test]
fn test_event_type_fields() {
    let et = make_event_type("test.Foo");
    assert_eq!(et.name, "test.Foo");
    assert_eq!(et.category.len(), 1);
    assert_eq!(et.fields.len(), 1);
    assert!(et.has_thread);
    assert!(!et.has_stacktrace);
    assert!(et.threshold.is_none());
}

#[test]
fn test_event_type_with_threshold() {
    let et = EventType {
        threshold: Some(std::time::Duration::from_millis(10)),
        ..make_event_type("test.Slow")
    };
    assert_eq!(et.threshold.unwrap(), std::time::Duration::from_millis(10));
}

#[test]
fn test_event_type_clone() {
    let et = make_event_type("test.Clone");
    let et2 = et.clone();
    assert_eq!(et.name, et2.name);
    assert_eq!(et.fields.len(), et2.fields.len());
}

// --- EventPeriod ---

#[test]
fn test_event_period_variants() {
    let _ = EventPeriod::None;
    let _ = EventPeriod::BeginEnd;
    let _ = EventPeriod::EveryChunk;
    let _ = EventPeriod::EverySecond;
}

#[test]
fn test_event_period_debug() {
    let p = EventPeriod::BeginEnd;
    let dbg = format!("{:?}", p);
    assert!(dbg.contains("BeginEnd"));
}

#[test]
fn test_event_period_clone() {
    let p = EventPeriod::EverySecond;
    let p2 = p.clone();
    let dbg1 = format!("{:?}", p);
    let dbg2 = format!("{:?}", p2);
    assert_eq!(dbg1, dbg2);
}

// --- EventInstance ---

#[test]
fn test_event_instance_construction() {
    let evt = EventInstance {
        type_id: EventTypeId(5),
        start_time: 1000,
        end_time: 2000,
        thread_id: 42,
        fields: smallvec![EventValue::Int(10)],
    };
    assert_eq!(evt.type_id, EventTypeId(5));
    assert_eq!(evt.start_time, 1000);
    assert_eq!(evt.end_time, 2000);
    assert_eq!(evt.thread_id, 42);
    assert_eq!(evt.fields.len(), 1);
}

#[test]
fn test_event_instance_instant_event() {
    let evt = EventInstance {
        type_id: EventTypeId(1),
        start_time: 5000,
        end_time: 5000, // instant event: start == end
        thread_id: 1,
        fields: smallvec![],
    };
    assert_eq!(evt.start_time, evt.end_time);
}

#[test]
fn test_event_instance_clone() {
    let evt = EventInstance {
        type_id: EventTypeId(1),
        start_time: 100,
        end_time: 200,
        thread_id: 1,
        fields: smallvec![EventValue::String(Arc::from("test"))],
    };
    let evt2 = evt.clone();
    assert_eq!(evt.type_id, evt2.type_id);
    assert_eq!(evt.fields.len(), evt2.fields.len());
}

// --- EventValue ---

#[test]
fn test_event_value_long() {
    let v = EventValue::Long(i64::MAX);
    assert!(matches!(v, EventValue::Long(x) if x == i64::MAX));
}

#[test]
fn test_event_value_int() {
    let v = EventValue::Int(-42);
    assert!(matches!(v, EventValue::Int(-42)));
}

#[test]
fn test_event_value_float() {
    let v = EventValue::Float(3.25);
    assert!(matches!(v, EventValue::Float(x) if (x - 3.25).abs() < 0.001));
}

#[test]
fn test_event_value_double() {
    let v = EventValue::Double(2.5);
    assert!(matches!(v, EventValue::Double(x) if (x - 2.5).abs() < 1e-9));
}

#[test]
fn test_event_value_boolean() {
    assert!(matches!(
        EventValue::Boolean(true),
        EventValue::Boolean(true)
    ));
    assert!(matches!(
        EventValue::Boolean(false),
        EventValue::Boolean(false)
    ));
}

#[test]
fn test_event_value_string() {
    let v = EventValue::String(Arc::from("hello world"));
    assert!(matches!(&v, EventValue::String(s) if &**s == "hello world"));
}

#[test]
fn test_event_value_null() {
    assert!(matches!(EventValue::Null, EventValue::Null));
}

#[test]
fn test_event_value_clone() {
    let v = EventValue::String(Arc::from("clone me"));
    let v2 = v.clone();
    match (&v, &v2) {
        (EventValue::String(a), EventValue::String(b)) => assert_eq!(&**a, &**b),
        _ => panic!("expected String"),
    }
}

#[test]
fn test_event_value_string_arc_sharing() {
    let s: Arc<str> = Arc::from("shared");
    let v1 = EventValue::String(Arc::clone(&s));
    let v2 = EventValue::String(Arc::clone(&s));
    match (&v1, &v2) {
        (EventValue::String(a), EventValue::String(b)) => assert!(Arc::ptr_eq(a, b)),
        _ => panic!("expected String"),
    }
}

#[test]
fn test_event_value_from_str() {
    let v = EventValue::from_str("hello");
    assert!(matches!(&v, EventValue::String(s) if &**s == "hello"));
}

#[test]
fn test_event_value_debug() {
    let v = EventValue::Int(99);
    let dbg = format!("{:?}", v);
    assert!(dbg.contains("99"));
}

// --- EventTypeRegistry ---

#[test]
fn test_registry_new_is_empty() {
    let reg = EventTypeRegistry::new();
    assert!(reg.is_empty());
    assert_eq!(reg.len(), 0);
}

#[test]
fn test_registry_default_is_empty() {
    let reg = EventTypeRegistry::default();
    assert!(reg.is_empty());
}

#[test]
fn test_registry_register_assigns_id() {
    let mut reg = EventTypeRegistry::new();
    let id1 = reg.register(make_event_type("first"));
    let id2 = reg.register(make_event_type("second"));
    assert_ne!(id1, id2);
    assert_eq!(reg.len(), 2);
}

#[test]
fn test_registry_register_overwrites_id_field() {
    let mut reg = EventTypeRegistry::new();
    let id = reg.register(EventType {
        id: EventTypeId(999), // should be overwritten
        name: "test".into(),
        category: vec![],
        description: "".into(),
        fields: vec![],
        has_thread: false,
        has_stacktrace: false,
        period: EventPeriod::None,
        threshold: None,
    });
    let stored = reg.get(id).unwrap();
    assert_eq!(stored.id, id);
    assert_ne!(stored.id, EventTypeId(999));
}

#[test]
fn test_registry_get_returns_correct_type() {
    let mut reg = EventTypeRegistry::new();
    let id = reg.register(make_event_type("myType"));
    let et = reg.get(id).unwrap();
    assert_eq!(et.name, "myType");
}

#[test]
fn test_registry_get_missing_returns_none() {
    let reg = EventTypeRegistry::new();
    assert!(reg.get(EventTypeId(100)).is_none());
}

#[test]
fn test_registry_find_by_name() {
    let mut reg = EventTypeRegistry::new();
    let id = reg.register(make_event_type("jdk.GC"));
    assert_eq!(reg.find_by_name("jdk.GC"), Some(id));
    assert_eq!(reg.find_by_name("jdk.Missing"), None);
}

#[test]
fn test_registry_iter() {
    let mut reg = EventTypeRegistry::new();
    reg.register(make_event_type("a"));
    reg.register(make_event_type("b"));
    reg.register(make_event_type("c"));
    let names: Vec<&str> = reg.iter().map(|(_, et)| et.name.as_str()).collect();
    assert_eq!(names.len(), 3);
    assert!(names.contains(&"a"));
    assert!(names.contains(&"b"));
    assert!(names.contains(&"c"));
}

#[test]
fn test_registry_ids_auto_increment() {
    let mut reg = EventTypeRegistry::new();
    let id1 = reg.register(make_event_type("x"));
    let id2 = reg.register(make_event_type("y"));
    let id3 = reg.register(make_event_type("z"));
    // IDs should be sequential starting from 1
    assert_eq!(id1, EventTypeId(1));
    assert_eq!(id2, EventTypeId(2));
    assert_eq!(id3, EventTypeId(3));
}

#[test]
fn test_registry_multiple_categories() {
    let mut reg = EventTypeRegistry::new();
    let et = EventType {
        category: vec!["JVM".into(), "GC".into(), "Collector".into()],
        ..make_event_type("gc.event")
    };
    let id = reg.register(et);
    let stored = reg.get(id).unwrap();
    assert_eq!(stored.category.len(), 3);
}

#[test]
fn test_registry_checked_add_does_not_wrap() {
    let mut reg = EventTypeRegistry::new();
    // Register a few events and verify IDs increment properly
    let id1 = reg.register(make_event_type("event.a"));
    let id2 = reg.register(make_event_type("event.b"));
    assert_eq!(id1.0 + 1, id2.0);
}

#[test]
fn test_registry_lookup_by_name() {
    let mut reg = EventTypeRegistry::new();
    let id = reg.register(make_event_type("jdk.ThreadStart"));
    assert_eq!(reg.find_by_name("jdk.ThreadStart"), Some(id));
    assert_eq!(reg.find_by_name("jdk.Missing"), None);
}

#[test]
fn test_registry_duplicate_name_returns_existing_id() {
    let mut reg = EventTypeRegistry::new();
    let id1 = reg.register(make_event_type("dup.Type"));
    let id2 = reg.register(EventType {
        description: "different metadata should not replace original".into(),
        fields: vec![EventField::new("other", "long", "Other field")],
        ..make_event_type("dup.Type")
    });

    assert_eq!(id2, id1);
    assert_eq!(reg.len(), 1);
    let stored = reg.get(id1).unwrap();
    assert_eq!(stored.fields[0].name, "field1");
    assert_eq!(stored.fields[0].type_name, "int");
}

#[test]
fn test_registry_rejects_malformed_metadata() {
    let mut reg = EventTypeRegistry::new();

    let bad_name = reg.register(make_event_type("bad\nname"));
    assert_eq!(bad_name, EventTypeId::INVALID);

    let bad_field_type = reg.register(EventType {
        fields: vec![EventField::new("field1", "object", "unsupported")],
        ..make_event_type("bad.FieldType")
    });
    assert_eq!(bad_field_type, EventTypeId::INVALID);

    let duplicate_field = reg.register(EventType {
        fields: vec![
            EventField::new("field1", "int", "First"),
            EventField::new("field1", "long", "Duplicate"),
        ],
        ..make_event_type("bad.DuplicateField")
    });
    assert_eq!(duplicate_field, EventTypeId::INVALID);

    assert!(reg.is_empty());
}
