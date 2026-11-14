//! The Celaut `Service` in SeSe.
//!
//! The mapping is generic: it follows the proto descriptor of `celaut.proto`
//! (compiled into the crate). For each proto message the SeSe message has one footer
//! entry per field number: **field number `n` is footer index `n - 1`**. Numbers that
//! the message does not use are [`sese::ABSENT`].
//!
//! | proto | SeSe |
//! |---|---|
//! | `bool` | bool |
//! | `int32`, `int64`, `sint*`, `sfixed*`, enum | u64: the value as i64, two's complement |
//! | `uint32`, `uint64`, `fixed*` | u64 |
//! | `float`, `double` | u64: the IEEE bits (a float in the low 32 bits) |
//! | `string` | string |
//! | `bytes` | bytes (block-aligned from 64 KiB) |
//! | message | nested message |
//! | `repeated` scalar / string / bytes / message | list |
//! | `map` | not supported (celaut.proto has none) |
//!
//! Presence: a field with no presence in proto3 (a plain scalar, string, bytes) is
//! always written, also when it has its default value. A field with presence (a
//! message, `optional`, a member of a `oneof`) is written only when set. A repeated
//! field is always written (its count can be 0).
//!
//! One `bytes` field holds a serialized message: `Service.Container.filesystem` holds a
//! `Filesystem`. In SeSe it is that message, nested, so each file of the service is a
//! `bytes` value of its own (block-aligned when large) and a reader walks the tree
//! with no proto parse. The proto bytes must be the canonical encoding of the
//! `Filesystem` (they are: the packer writes them in field order), so that the way back
//! gives the same bytes and the same Celaut service id.
//!
//! The way back ([`sese_to_service`]) gives the proto bytes of the service. For a
//! service in canonical proto form, `sese_to_service(service_to_sese(p)) == p`, so the
//! Celaut id (SHA3-256 of the proto bytes) is kept.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::sync::OnceLock;

use prost::Message as _;
use prost_reflect::{
    DescriptorPool, DynamicMessage, FieldDescriptor, Kind, MessageDescriptor, ReflectMessage, Value,
};
use sese::{Message, Writer, WriterOptions, MAX_DEPTH};

/// The full name of the `bytes` field that holds a serialized `Filesystem`.
pub const FILESYSTEM_FIELD: &str = "celaut.Service.Container.filesystem";
/// The full name of the `Filesystem` message.
pub const FILESYSTEM: &str = "celaut.Service.Container.Filesystem";
/// The full name of the `Service` message.
pub const SERVICE: &str = "celaut.Service";

/// The descriptors of celaut.proto and buffer.proto (nodo `dev`, see `protos/`).
pub fn pool() -> &'static DescriptorPool {
    static POOL: OnceLock<DescriptorPool> = OnceLock::new();
    POOL.get_or_init(|| {
        #[allow(clippy::expect_used)]
        DescriptorPool::decode(&include_bytes!(concat!(env!("OUT_DIR"), "/celaut.bin"))[..])
            .expect("the compiled descriptor set is valid")
    })
}

/// The descriptor of a message of celaut.proto by full name.
pub fn descriptor(name: &str) -> Result<MessageDescriptor, Error> {
    pool()
        .get_message_by_name(name)
        .ok_or_else(|| Error::Schema(format!("no message {name}")))
}

/// Errors of the mapping.
#[derive(Debug)]
pub enum Error {
    /// The SeSe reader or writer refused.
    Sese(sese::Error),
    /// The proto bytes do not decode.
    Proto(String),
    /// The input cannot be mapped without loss (unknown fields, a map, a filesystem that
    /// is not in canonical form, two members of one oneof, a value out of range).
    Lossy(String),
    /// A schema problem.
    Schema(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Sese(e) => write!(f, "sese: {e}"),
            Error::Proto(e) => write!(f, "proto: {e}"),
            Error::Lossy(e) => write!(f, "not mappable without loss: {e}"),
            Error::Schema(e) => write!(f, "schema: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<sese::Error> for Error {
    fn from(e: sese::Error) -> Self {
        Error::Sese(e)
    }
}

/// The number of footer entries of a message: its largest field number.
pub fn footer_len(desc: &MessageDescriptor) -> usize {
    desc.fields()
        .map(|f| f.number() as usize)
        .max()
        .unwrap_or(0)
}

fn index(f: &FieldDescriptor) -> usize {
    f.number() as usize - 1
}

/// Proto bytes of a `Service` to canonical SeSe.
pub fn service_to_sese(proto: &[u8]) -> Result<Vec<u8>, Error> {
    proto_to_sese(&descriptor(SERVICE)?, proto)
}

/// SeSe of a `Service` to its proto bytes.
pub fn sese_to_service(sese: &[u8]) -> Result<Vec<u8>, Error> {
    sese_to_proto(&descriptor(SERVICE)?, sese)
}

/// Proto bytes of any message of celaut.proto to canonical SeSe.
pub fn proto_to_sese(desc: &MessageDescriptor, proto: &[u8]) -> Result<Vec<u8>, Error> {
    proto_to_sese_with(desc, proto, WriterOptions::default())
}

/// [`proto_to_sese`] with other writer options. The result is canonical only with
/// [`WriterOptions::default`]; other options are for experiments (block size choice).
pub fn proto_to_sese_with(
    desc: &MessageDescriptor,
    proto: &[u8],
    opts: WriterOptions,
) -> Result<Vec<u8>, Error> {
    let msg =
        DynamicMessage::decode(desc.clone(), proto).map_err(|e| Error::Proto(e.to_string()))?;
    message_to_sese_with(&msg, proto.len() + proto.len() / 8 + 4096, opts)
}

/// A decoded proto message to canonical SeSe. `capacity` is a hint for the output size.
pub fn message_to_sese(msg: &DynamicMessage, capacity: usize) -> Result<Vec<u8>, Error> {
    message_to_sese_with(msg, capacity, WriterOptions::default())
}

/// [`message_to_sese`] with other writer options (see [`proto_to_sese_with`]).
pub fn message_to_sese_with(
    msg: &DynamicMessage,
    capacity: usize,
    opts: WriterOptions,
) -> Result<Vec<u8>, Error> {
    let mut w = Writer::with_capacity(opts, capacity);
    w.begin(footer_len(&msg.descriptor()))?;
    write_fields(&mut w, msg, 0)?;
    w.end()?;
    Ok(w.finish()?)
}

/// SeSe of any message of celaut.proto to its proto bytes.
pub fn sese_to_proto(desc: &MessageDescriptor, sese: &[u8]) -> Result<Vec<u8>, Error> {
    Ok(sese_to_message(desc, &Message::root(sese)?)?.encode_to_vec())
}

/// One SeSe message to a decoded proto message.
pub fn sese_to_message(desc: &MessageDescriptor, m: &Message<'_>) -> Result<DynamicMessage, Error> {
    read_fields(desc, m, 0)
}

fn sorted_fields(desc: &MessageDescriptor) -> Vec<FieldDescriptor> {
    let mut v: Vec<_> = desc.fields().collect();
    v.sort_by_key(FieldDescriptor::number);
    v
}

fn scalar_u64(v: &Value) -> Option<u64> {
    Some(match v {
        Value::I32(x) => i64::from(*x) as u64,
        Value::I64(x) => *x as u64,
        Value::U32(x) => u64::from(*x),
        Value::U64(x) => *x,
        Value::F32(x) => u64::from(x.to_bits()),
        Value::F64(x) => x.to_bits(),
        Value::EnumNumber(x) => i64::from(*x) as u64,
        _ => return None,
    })
}

fn write_fields(w: &mut Writer, msg: &DynamicMessage, depth: usize) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Err(sese::Error::TooDeep.into());
    }
    if msg.unknown_fields().next().is_some() {
        return Err(Error::Lossy(format!(
            "unknown fields in {}",
            msg.descriptor().full_name()
        )));
    }
    for f in sorted_fields(&msg.descriptor()) {
        let i = index(&f);
        if f.is_map() {
            return Err(Error::Lossy(format!("map field {}", f.full_name())));
        }
        if f.is_list() {
            let v = msg.get_field(&f);
            let list = v
                .as_list()
                .ok_or_else(|| Error::Schema(f.full_name().into()))?;
            write_list(w, i, &f, list, depth)?;
            continue;
        }
        if f.supports_presence() && !msg.has_field(&f) {
            continue;
        }
        let v = msg.get_field(&f);
        match (f.kind(), v.as_ref()) {
            (Kind::Bool, Value::Bool(b)) => w.bool(i, *b)?,
            (Kind::String, Value::String(s)) => w.str(i, s)?,
            (Kind::Bytes, Value::Bytes(b)) if f.full_name() == FILESYSTEM_FIELD => {
                let fs_desc = descriptor(FILESYSTEM)?;
                let fs = DynamicMessage::decode(fs_desc, b.as_ref())
                    .map_err(|e| Error::Lossy(format!("filesystem does not decode: {e}")))?;
                if fs.encoded_len() != b.len() || fs.encode_to_vec() != b.as_ref() {
                    return Err(Error::Lossy(
                        "filesystem is not in canonical proto form".into(),
                    ));
                }
                w.begin_message(i, footer_len(&fs.descriptor()))?;
                write_fields(w, &fs, depth + 1)?;
                w.end()?;
            }
            (Kind::Bytes, Value::Bytes(b)) => w.bytes(i, b)?,
            (Kind::Message(d), Value::Message(m)) => {
                w.begin_message(i, footer_len(&d))?;
                write_fields(w, m, depth + 1)?;
                w.end()?;
            }
            (_, other) => {
                let x = scalar_u64(other)
                    .ok_or_else(|| Error::Schema(format!("value of {}", f.full_name())))?;
                w.u64(i, x)?;
            }
        }
    }
    Ok(())
}

fn write_list(
    w: &mut Writer,
    i: usize,
    f: &FieldDescriptor,
    list: &[Value],
    depth: usize,
) -> Result<(), Error> {
    match f.kind() {
        Kind::Bool => {
            let v: Vec<bool> = list.iter().map(|x| x.as_bool().unwrap_or(false)).collect();
            w.list_bool(i, &v)?;
        }
        Kind::String => {
            w.begin_list(i, list.len() as u64)?;
            for x in list {
                w.element_str(
                    x.as_str()
                        .ok_or_else(|| Error::Schema(f.full_name().into()))?,
                )?;
            }
            w.end()?;
        }
        Kind::Bytes => {
            w.begin_list(i, list.len() as u64)?;
            for x in list {
                w.element_bytes(
                    x.as_bytes()
                        .ok_or_else(|| Error::Schema(f.full_name().into()))?,
                )?;
            }
            w.end()?;
        }
        Kind::Message(d) => {
            w.begin_list(i, list.len() as u64)?;
            for x in list {
                let m = x
                    .as_message()
                    .ok_or_else(|| Error::Schema(f.full_name().into()))?;
                w.begin_element_message(footer_len(&d))?;
                write_fields(w, m, depth + 1)?;
                w.end()?;
            }
            w.end()?;
        }
        _ => {
            let v = list
                .iter()
                .map(|x| scalar_u64(x).ok_or_else(|| Error::Schema(f.full_name().into())))
                .collect::<Result<Vec<u64>, Error>>()?;
            w.list_u64(i, &v)?;
        }
    }
    Ok(())
}

fn scalar_from_u64(kind: &Kind, x: u64, name: &str) -> Result<Value, Error> {
    let range = || Error::Lossy(format!("{name}: {x:#x} is out of range for its proto type"));
    Ok(match kind {
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => {
            Value::I32(i32::try_from(x as i64).map_err(|_| range())?)
        }
        Kind::Enum(_) => Value::EnumNumber(i32::try_from(x as i64).map_err(|_| range())?),
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => Value::I64(x as i64),
        Kind::Uint32 | Kind::Fixed32 => Value::U32(u32::try_from(x).map_err(|_| range())?),
        Kind::Uint64 | Kind::Fixed64 => Value::U64(x),
        Kind::Float => Value::F32(f32::from_bits(u32::try_from(x).map_err(|_| range())?)),
        Kind::Double => Value::F64(f64::from_bits(x)),
        _ => return Err(Error::Schema(format!("{name} is not a scalar"))),
    })
}

fn is_default(v: &Value) -> bool {
    match v {
        Value::Bool(b) => !b,
        Value::I32(x) | Value::EnumNumber(x) => *x == 0,
        Value::I64(x) => *x == 0,
        Value::U32(x) => *x == 0,
        Value::U64(x) => *x == 0,
        // Proto3 omits +0.0 only; -0.0 is written. Compare bits.
        Value::F32(x) => x.to_bits() == 0,
        Value::F64(x) => x.to_bits() == 0,
        Value::String(s) => s.is_empty(),
        Value::Bytes(b) => b.is_empty(),
        _ => false,
    }
}

fn read_fields(
    desc: &MessageDescriptor,
    m: &Message<'_>,
    depth: usize,
) -> Result<DynamicMessage, Error> {
    if depth > MAX_DEPTH {
        return Err(sese::Error::TooDeep.into());
    }
    let mut out = DynamicMessage::new(desc.clone());
    let mut oneofs_seen: Vec<String> = Vec::new();
    for f in sorted_fields(desc) {
        let i = index(&f);
        if m.offset(i)?.is_none() {
            continue;
        }
        if f.is_map() {
            return Err(Error::Lossy(format!("map field {}", f.full_name())));
        }
        if let Some(o) = f.containing_oneof() {
            let name = o.full_name().to_string();
            if oneofs_seen.contains(&name) {
                return Err(Error::Lossy(format!("two members of oneof {name}")));
            }
            oneofs_seen.push(name);
        }
        let name = f.full_name();
        if f.is_list() {
            let l = m.list(i)?.ok_or_else(|| Error::Schema(name.into()))?;
            let values: Vec<Value> = match f.kind() {
                Kind::Bool => l.bools()?.into_iter().map(Value::Bool).collect(),
                Kind::String => {
                    sese::value::elements(&l, |l, k| Ok(Value::String(l.str(k)?.to_string())))?
                }
                Kind::Bytes => {
                    sese::value::elements(&l, |l, k| Ok(Value::Bytes(l.bytes(k)?.to_vec().into())))?
                }
                Kind::Message(d) => {
                    l.check_offsets()?;
                    let mut v = Vec::new();
                    for k in 0..l.count() {
                        v.push(Value::Message(read_fields(&d, &l.message(k)?, depth + 1)?));
                    }
                    v
                }
                kind => l
                    .u64s()?
                    .map(|x| scalar_from_u64(&kind, x, name))
                    .collect::<Result<_, _>>()?,
            };
            if !values.is_empty() {
                out.set_field(&f, Value::List(values));
            }
            continue;
        }
        let v = match f.kind() {
            Kind::Bool => Value::Bool(m.bool(i)?.unwrap_or(false)),
            Kind::String => Value::String(m.str(i)?.unwrap_or("").to_string()),
            Kind::Bytes if name == FILESYSTEM_FIELD => {
                let child = m.message(i)?.ok_or_else(|| Error::Schema(name.into()))?;
                let fs = read_fields(&descriptor(FILESYSTEM)?, &child, depth + 1)?;
                Value::Bytes(fs.encode_to_vec().into())
            }
            Kind::Bytes => Value::Bytes(m.bytes(i)?.unwrap_or(&[]).to_vec().into()),
            Kind::Message(d) => {
                let child = m.message(i)?.ok_or_else(|| Error::Schema(name.into()))?;
                Value::Message(read_fields(&d, &child, depth + 1)?)
            }
            kind => scalar_from_u64(&kind, m.u64(i)?.unwrap_or(0), name)?,
        };
        if !f.supports_presence() && is_default(&v) {
            continue;
        }
        out.set_field(&f, v);
    }
    Ok(out)
}

/// What a filesystem entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry<'a> {
    /// A file: its content, a slice of the SeSe input (no copy).
    File(&'a [u8]),
    /// A symbolic link to `src`.
    Link(&'a str),
    /// A directory.
    Dir,
    /// An entry with none of the three (a device, or an empty file in proto form).
    Empty,
}

/// Walk the filesystem of a Service in SeSe, with no copy and no proto parse:
/// `f(path, entry)` for each entry, parents before children. Paths are joined with `/`.
pub fn walk_filesystem<'a>(
    service: &Message<'a>,
    mut f: impl FnMut(&str, Entry<'a>),
) -> Result<(), Error> {
    // Service.container = 2, Container.filesystem = 2.
    let Some(container) = service.message(1)? else {
        return Ok(());
    };
    let Some(fs) = container.message(1)? else {
        return Ok(());
    };
    walk_dir(&fs, "", &mut f, 0)
}

fn walk_dir<'a>(
    fs: &Message<'a>,
    prefix: &str,
    f: &mut impl FnMut(&str, Entry<'a>),
    depth: usize,
) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Err(sese::Error::TooDeep.into());
    }
    // Filesystem.branch = 1; ItemBranch: name 1, file 2, link 3, filesystem 4.
    let Some(branches) = fs.list(0)? else {
        return Ok(());
    };
    branches.check_offsets()?;
    for k in 0..branches.count() {
        let b = branches.message(k)?;
        let name = b.str(0)?.unwrap_or("");
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        if let Some(data) = b.bytes(1)? {
            f(&path, Entry::File(data));
        } else if let Some(link) = b.message(2)? {
            f(&path, Entry::Link(link.str(0)?.unwrap_or("")));
        } else if let Some(sub) = b.message(3)? {
            f(&path, Entry::Dir);
            walk_dir(&sub, &path, f, depth + 1)?;
        } else {
            f(&path, Entry::Empty);
        }
    }
    Ok(())
}
