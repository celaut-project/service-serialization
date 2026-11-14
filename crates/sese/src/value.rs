//! A value tree and a schema: generic encode and decode, for tools and tests.
//!
//! A SeSe file does not describe itself: the reader needs the schema to know the type
//! of each field. [`Schema`] gives it. [`encode`] writes a tree with the canonical
//! writer; [`decode`] reads a file with a schema; [`is_canonical`] checks that a file is
//! the canonical encoding of what it holds.

use crate::read::{List, Message};
use crate::write::{Writer, WriterOptions};
use crate::{Error, MAX_DEPTH};

/// The type of a field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Type {
    /// 1 byte.
    Bool,
    /// 8 bytes.
    U64,
    /// u64 length, then the bytes.
    Bytes,
    /// u64 length, then UTF-8.
    Str,
    /// u64 length, then a nested message.
    Message(Schema),
    /// u64 count, then 1 byte each.
    ListBool,
    /// u64 count, then 8 bytes each.
    ListU64,
    /// u64 count, offsets, then bytes values.
    ListBytes,
    /// u64 count, offsets, then string values.
    ListStr,
    /// u64 count, offsets, then messages.
    ListMessage(Schema),
}

/// The fields of a message, by index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Schema {
    /// The type of each field.
    pub fields: Vec<Type>,
}

/// A value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// Not present.
    Absent,
    /// A bool.
    Bool(bool),
    /// A u64.
    U64(u64),
    /// Bytes.
    Bytes(Vec<u8>),
    /// A string.
    Str(String),
    /// A nested message: one value per field index.
    Message(Vec<Value>),
    /// A list of bool.
    ListBool(Vec<bool>),
    /// A list of u64.
    ListU64(Vec<u64>),
    /// A list of bytes.
    ListBytes(Vec<Vec<u8>>),
    /// A list of strings.
    ListStr(Vec<String>),
    /// A list of messages.
    ListMessage(Vec<Vec<Value>>),
}

/// Encode `fields` as a root message with the canonical writer (or `opts`).
pub fn encode(fields: &[Value], opts: WriterOptions) -> Result<Vec<u8>, Error> {
    let mut w = Writer::new(opts);
    w.begin(fields.len())?;
    write_fields(&mut w, fields, 0)?;
    w.end()?;
    w.finish()
}

fn write_fields(w: &mut Writer, fields: &[Value], depth: usize) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Err(Error::TooDeep);
    }
    for (i, v) in fields.iter().enumerate() {
        match v {
            Value::Absent => {}
            Value::Bool(b) => w.bool(i, *b)?,
            Value::U64(x) => w.u64(i, *x)?,
            Value::Bytes(b) => w.bytes(i, b)?,
            Value::Str(s) => w.str(i, s)?,
            Value::Message(m) => {
                w.begin_message(i, m.len())?;
                write_fields(w, m, depth + 1)?;
                w.end()?;
            }
            Value::ListBool(l) => w.list_bool(i, l)?,
            Value::ListU64(l) => w.list_u64(i, l)?,
            Value::ListBytes(l) => {
                w.begin_list(i, l.len() as u64)?;
                for e in l {
                    w.element_bytes(e)?;
                }
                w.end()?;
            }
            Value::ListStr(l) => {
                w.begin_list(i, l.len() as u64)?;
                for e in l {
                    w.element_str(e)?;
                }
                w.end()?;
            }
            Value::ListMessage(l) => {
                w.begin_list(i, l.len() as u64)?;
                for e in l {
                    w.begin_element_message(e.len())?;
                    write_fields(w, e, depth + 1)?;
                    w.end()?;
                }
                w.end()?;
            }
        }
    }
    Ok(())
}

/// Decode the root message of `buf` with `schema`. A field past the footer is absent;
/// footer entries past the schema are ignored.
pub fn decode(buf: &[u8], schema: &Schema) -> Result<Vec<Value>, Error> {
    read_fields(&Message::root(buf)?, schema, 0)
}

/// Decode one message with `schema`.
pub fn decode_message(m: &Message<'_>, schema: &Schema) -> Result<Vec<Value>, Error> {
    read_fields(m, schema, 0)
}

fn read_fields(m: &Message<'_>, schema: &Schema, depth: usize) -> Result<Vec<Value>, Error> {
    if depth > MAX_DEPTH {
        return Err(Error::TooDeep);
    }
    let mut out = Vec::with_capacity(schema.fields.len());
    for (i, t) in schema.fields.iter().enumerate() {
        if m.offset(i)?.is_none() {
            out.push(Value::Absent);
            continue;
        }
        let v = match t {
            Type::Bool => m.bool(i)?.map(Value::Bool),
            Type::U64 => m.u64(i)?.map(Value::U64),
            Type::Bytes => m.bytes(i)?.map(|b| Value::Bytes(b.to_vec())),
            Type::Str => m.str(i)?.map(|s| Value::Str(s.to_string())),
            Type::Message(s) => match m.message(i)? {
                Some(c) => Some(Value::Message(read_fields(&c, s, depth + 1)?)),
                None => None,
            },
            Type::ListBool => m
                .list(i)?
                .map(|l| l.bools().map(Value::ListBool))
                .transpose()?,
            Type::ListU64 => m
                .list(i)?
                .map(|l| l.u64s().map(|it| Value::ListU64(it.collect())))
                .transpose()?,
            Type::ListBytes => m
                .list(i)?
                .map(|l| elements(&l, |l, k| l.bytes(k).map(<[u8]>::to_vec)).map(Value::ListBytes))
                .transpose()?,
            Type::ListStr => m
                .list(i)?
                .map(|l| elements(&l, |l, k| l.str(k).map(str::to_string)).map(Value::ListStr))
                .transpose()?,
            Type::ListMessage(s) => m
                .list(i)?
                .map(|l| {
                    elements(&l, |l, k| read_fields(&l.message(k)?, s, depth + 1))
                        .map(Value::ListMessage)
                })
                .transpose()?,
        };
        out.push(v.unwrap_or(Value::Absent));
    }
    Ok(out)
}

/// All elements of a variable-size list, after a check that its offset table fits.
pub fn elements<'a, T>(
    l: &List<'a>,
    mut f: impl FnMut(&List<'a>, u64) -> Result<T, Error>,
) -> Result<Vec<T>, Error> {
    l.check_offsets()?;
    let mut out = Vec::with_capacity(usize::try_from(l.count()).unwrap_or(0));
    for k in 0..l.count() {
        out.push(f(l, k)?);
    }
    Ok(out)
}

/// True if `buf` decodes with `schema` and the canonical writer gives the same bytes
/// back. Two canonical files with the same value are equal, so their ids are equal.
pub fn is_canonical(buf: &[u8], schema: &Schema) -> Result<bool, Error> {
    let root = Message::root(buf)?;
    if root.field_count() != schema.fields.len() as u64 {
        return Ok(false);
    }
    let v = read_fields(&root, schema, 0)?;
    Ok(encode(&v, WriterOptions::default())? == buf)
}

/// The schema that `fields` has. An absent field gets [`Type::Bool`] (any type reads an
/// absent field). A list of messages takes the schema of its first element.
pub fn schema_of(fields: &[Value]) -> Schema {
    Schema {
        fields: fields
            .iter()
            .map(|v| match v {
                Value::Absent | Value::Bool(_) => Type::Bool,
                Value::U64(_) => Type::U64,
                Value::Bytes(_) => Type::Bytes,
                Value::Str(_) => Type::Str,
                Value::Message(m) => Type::Message(schema_of(m)),
                Value::ListBool(_) => Type::ListBool,
                Value::ListU64(_) => Type::ListU64,
                Value::ListBytes(_) => Type::ListBytes,
                Value::ListStr(_) => Type::ListStr,
                Value::ListMessage(l) => {
                    Type::ListMessage(l.first().map(|m| schema_of(m)).unwrap_or_default())
                }
            })
            .collect(),
    }
}
