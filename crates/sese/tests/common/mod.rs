#![allow(dead_code)]
use sese::value::{Schema, Type, Value};

/// The golden vectors: name, value.
pub fn vectors() -> Vec<(&'static str, Vec<Value>)> {
    vec![
        ("empty", vec![]),
        (
            "scalars",
            vec![
                Value::Bool(true),
                Value::U64(42),
                Value::Absent,
                Value::Str("hi".into()),
                Value::Bytes(vec![1, 2, 3]),
            ],
        ),
        (
            "nested",
            vec![
                Value::Str("outer".into()),
                Value::Message(vec![Value::U64(7), Value::Str("inner".into())]),
                Value::ListU64(vec![1, 2, 3]),
                Value::ListStr(vec!["a".into(), "bc".into()]),
                Value::ListMessage(vec![vec![Value::U64(1)], vec![Value::U64(2)]]),
                Value::ListBool(vec![true, false]),
                Value::ListBytes(vec![vec![], vec![0xff]]),
            ],
        ),
        (
            "aligned",
            vec![
                Value::Str("x".into()),
                Value::Bytes((0..70_000u32).map(|i| (i % 251) as u8).collect()),
                Value::U64(9),
            ],
        ),
    ]
}

pub fn schema_for(name: &str) -> Schema {
    let v = vectors().into_iter().find(|(n, _)| *n == name).unwrap().1;
    let mut s = sese::value::schema_of(&v);
    if name == "scalars" {
        s.fields[2] = Type::U64;
    }
    s
}
