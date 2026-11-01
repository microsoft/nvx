use serde::de::IntoDeserializer;
use serde_json::json;

use super::*;

#[test]
fn repeated_fields_name_their_object() {
    for (text, object, field) in [
        (r#"{"deny": [], "deny": []}"#, "egress policy", "deny"),
        (
            r#"{"allow": [{"ports": [{}, {"port": 1, "port": 2}]}]}"#,
            "allow[0].ports[1]",
            "port",
        ),
        (r#"{"a b": {"x": 1, "x": 2}}"#, r#"["a b"]"#, "x"),
        (r#"{"a": {"": {"x": 1, "\u0078": 2}}}"#, r#"a[""]"#, "x"),
        (r#"[[{"_1": {"x": 1, "x": 2}}]]"#, "[0][0]._1", "x"),
    ] {
        match read(text) {
            Err(ReadError::Repeated {
                object: path,
                field: name,
            }) => assert_eq!((path.as_str(), name.as_str()), (object, field), "{text}"),
            other => panic!("{text}: {other:?}"),
        }
    }
}

#[test]
fn nesting_is_limited() {
    let nested = |depth: usize| format!("{}{}", "[".repeat(depth), "]".repeat(depth));
    assert!(read(&nested(MAX_JSON_DEPTH)).is_ok());
    match read(&nested(MAX_JSON_DEPTH + 1)) {
        // `serde_json` reports a position just past the bracket that opens the array.
        Err(ReadError::TooDeep { line, column }) => {
            assert_eq!(line, 1);
            assert!((MAX_JSON_DEPTH + 1..=MAX_JSON_DEPTH + 2).contains(&column));
        }
        other => panic!("{other:?}"),
    }
    let objects = |depth: usize| format!("{}1{}", r#"{"a": "#.repeat(depth), "}".repeat(depth));
    assert!(read(&objects(MAX_JSON_DEPTH)).is_ok());
    assert!(matches!(
        read(&objects(MAX_JSON_DEPTH + 1)),
        Err(ReadError::TooDeep { .. })
    ));
    // Far deeper than `serde_json`'s own limit, which the limit precedes.
    assert!(matches!(
        read(&nested(100_000)),
        Err(ReadError::TooDeep { .. })
    ));
}

#[test]
fn only_rfc_8259_json_is_read() {
    for text in [
        "",
        "{",
        "NaN",
        "[Infinity]",
        "[-Infinity]",
        "[1e400]",
        r#"["\ud800"]"#,
        "\u{feff}{}",
        "{} {}",
        "{},",
        "[1,]",
        "{'a': 1}",
        "\u{c}{}",
    ] {
        assert!(matches!(read(text), Err(ReadError::Syntax(_))), "{text:?}");
    }
    assert_eq!(read(" {\"a\": [1, -2, 2.5]}\n").unwrap()["a"][2], 2.5);
}

#[test]
fn values_read_as_serde_json_reads_them() {
    for text in [
        "null",
        "[true, false, null]",
        r#"{"a": [0, -1, 18446744073709551615, -9223372036854775808, 1.5, 1e300]}"#,
        r#"{"a\u0062": "\u00e9\ud83d\ude00\n", "": {}, "x": [[], {"y": [true]}]}"#,
    ] {
        let expected: Value = serde_json::from_str(text).unwrap();
        assert_eq!(read(text).unwrap(), expected, "{text}");
    }
    // As `serde_json` reads `-0` without `arbitrary_precision`, which reads the integer 0.
    assert_eq!(read("-0").unwrap(), Value::from(-0.0));
    // An object with the field in which `serde_json` hands numbers with `arbitrary_precision`
    // stays an object, with the feature or without it.
    let port = read(r#"{"port": {"$serde_json::private::Number": "1e400"}}"#).unwrap();
    assert_eq!(port["port"][NUMBER_FIELD], "1e400");
}

#[test]
fn documents_name_their_root() {
    let refused = read_document(r#"{"format": 1, "format": 1}"#, "manifest");
    assert!(matches!(
        refused,
        Err(ReadError::Repeated { object, field }) if object == "manifest" && field == "format"
    ));
    let refused = read_document(r#"{"packages": [{"name": "a", "name": "b"}]}"#, "manifest");
    assert!(matches!(
        refused,
        Err(ReadError::Repeated { object, field }) if object == "packages[0]" && field == "name"
    ));
}

#[test]
fn objects_with_the_number_field_stay_objects_with_arbitrary_precision() {
    use serde::de::value::{Error, MapDeserializer};

    // The `format` of a distro manifest that is `{"$serde_json::private::Number": "1"}`, whose
    // string `serde_json` hands borrowed or copied, as of every document, never owned: an
    // object, which no integer check accepts, where `serde_json`'s own `Value` would read the
    // number 1 with `arbitrary_precision`.
    let refusal = RefCell::new(None);
    let seed = Seed {
        location: Location::Root("manifest"),
        depth: 0,
        refusal: &refusal,
        arbitrary_precision: true,
    };
    let format = MapDeserializer::<_, Error>::new([(NUMBER_FIELD, "1")].into_iter());
    let format = seed.deserialize(format).unwrap();
    assert_eq!(format, json!({"$serde_json::private::Number": "1"}));
    assert_eq!(format.as_u64(), None);
    // The number 1 that `serde_json` reads with `arbitrary_precision`, an integer that it
    // hands as one.
    assert_eq!(seed.visit_u64::<Error>(1).unwrap().as_u64(), Some(1));
    assert!(refusal.into_inner().is_none());
}

#[test]
fn numbers_read_alike_with_arbitrary_precision() {
    use serde::de::value::{Error, MapDeserializer};

    let refusal = RefCell::new(None);
    let seed = |depth| Seed {
        location: Location::Root("egress policy"),
        depth,
        refusal: &refusal,
        arbitrary_precision: true,
    };
    // A number as `serde_json` hands it with `arbitrary_precision`, its text an owned string,
    // as deep as values may be, which an object there would not be.
    let handed = |text: &str| {
        seed(MAX_JSON_DEPTH).deserialize(MapDeserializer::<_, Error>::new(
            [(NUMBER_FIELD, text.to_owned())].into_iter(),
        ))
    };
    for (text, double) in [
        ("1.5", 1.5),
        ("-0.0", -0.0),
        ("1e300", 1e300),
        ("18446744073709551616", 2f64.powi(64)),
        ("-9223372036854775809", -(2f64.powi(63))),
    ] {
        assert_eq!(handed(text).unwrap(), Value::from(double), "{text}");
    }
    let error = handed("1e400").unwrap_err();
    assert!(error.to_string().contains("number out of range"), "{error}");
    let zero = seed(0).visit_i64::<Error>(0).unwrap();
    assert!(zero.as_f64().is_some_and(f64::is_sign_negative), "{zero}");
    assert_eq!(seed(0).visit_i64::<Error>(-1).unwrap(), Value::from(-1));
    // An object of the document with that field, whose strings `serde_json` hands borrowed or
    // copied, never owned.
    let members = MapDeserializer::<_, Error>::new([(NUMBER_FIELD, "1e400")].into_iter());
    assert_eq!(
        seed(0).deserialize(members).unwrap(),
        json!({"$serde_json::private::Number": "1e400"})
    );
    let members = MapDeserializer::<_, Error>::new([(NUMBER_FIELD, 7u64), ("x", 8)].into_iter());
    assert_eq!(
        seed(0).deserialize(members).unwrap(),
        json!({"$serde_json::private::Number": 7, "x": 8})
    );
    assert!(refusal.borrow().is_none());
    let members =
        MapDeserializer::<_, Error>::new([(NUMBER_FIELD, 1u64), (NUMBER_FIELD, 2)].into_iter());
    assert!(seed(0).deserialize(members).is_err());
    assert!(matches!(
        refusal.replace(None),
        Some(Refusal::Repeated { field, .. }) if field == NUMBER_FIELD
    ));
    let members = MapDeserializer::<_, Error>::new([(NUMBER_FIELD, "1")].into_iter());
    assert!(seed(MAX_JSON_DEPTH).deserialize(members).is_err());
    assert!(matches!(refusal.replace(None), Some(Refusal::TooDeep)));
    // The value of that field lies within the object, one level deeper.
    let nested = |depth, members: Members| {
        let object = MapDeserializer::<_, Error>::new([(NUMBER_FIELD, members)].into_iter());
        seed(depth).deserialize(object)
    };
    assert!(nested(0, Members(vec![("x", 1), ("x", 2)])).is_err());
    assert!(matches!(
        refusal.replace(None),
        Some(Refusal::Repeated { object, field })
            if object == r#"["$serde_json::private::Number"]"# && field == "x"
    ));
    assert!(nested(MAX_JSON_DEPTH - 2, Members(vec![("x", 1)])).is_ok());
    assert!(nested(MAX_JSON_DEPTH - 1, Members(vec![("x", 1)])).is_err());
    assert!(matches!(refusal.into_inner(), Some(Refusal::TooDeep)));
}

/// The fields of a map, which may repeat, to hand as a value of a field of another map.
struct Members(Vec<(&'static str, u64)>);

impl<'de> IntoDeserializer<'de, serde::de::value::Error> for Members {
    type Deserializer = serde::de::value::MapDeserializer<
        'de,
        std::vec::IntoIter<(&'static str, u64)>,
        serde::de::value::Error,
    >;

    fn into_deserializer(self) -> Self::Deserializer {
        serde::de::value::MapDeserializer::new(self.0.into_iter())
    }
}
