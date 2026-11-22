//! AMF0, the little of it RTMP commands and metadata need.

use serde_json::{Map, Value, json};

#[derive(Debug, Clone, PartialEq)]
pub enum Amf {
    Number(f64),
    Bool(bool),
    Str(String),
    Object(Vec<(String, Amf)>),
    /// A strict array (Enhanced RTMP's `fourCcList`).
    Array(Vec<Amf>),
    Null,
}

impl Amf {
    pub fn str(&self) -> Option<&str> {
        if let Amf::Str(s) = self { Some(s) } else { None }
    }
    pub fn num(&self) -> Option<f64> {
        if let Amf::Number(n) = self { Some(*n) } else { None }
    }
    pub fn to_json(&self) -> Value {
        match self {
            Amf::Number(n) => json!(n),
            Amf::Bool(b) => json!(b),
            Amf::Str(s) => json!(s),
            Amf::Null => Value::Null,
            Amf::Object(f) => Value::Object(f.iter().map(|(k, v)| (k.clone(), v.to_json())).collect::<Map<_, _>>()),
            Amf::Array(a) => Value::Array(a.iter().map(Amf::to_json).collect()),
        }
    }
}

pub fn write(out: &mut Vec<u8>, v: &Amf) {
    match v {
        Amf::Number(n) => {
            out.push(0);
            out.extend(n.to_be_bytes());
        }
        Amf::Bool(b) => out.extend([1, *b as u8]),
        Amf::Str(s) => {
            out.push(2);
            out.extend((s.len() as u16).to_be_bytes());
            out.extend(s.as_bytes());
        }
        Amf::Object(fields) => {
            out.push(3);
            for (k, v) in fields {
                out.extend((k.len() as u16).to_be_bytes());
                out.extend(k.as_bytes());
                write(out, v);
            }
            out.extend([0, 0, 9]);
        }
        Amf::Array(items) => {
            out.push(10);
            out.extend((items.len() as u32).to_be_bytes());
            for v in items {
                write(out, v);
            }
        }
        Amf::Null => out.push(5),
    }
}

pub fn obj(fields: &[(&str, Amf)]) -> Amf {
    Amf::Object(fields.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
}

pub fn s(v: &str) -> Amf {
    Amf::Str(v.to_string())
}

/// The values of a message, in order; what it can't read ends the list.
pub fn read_all(mut b: &[u8]) -> Vec<Amf> {
    let mut out = vec![];
    while let Some(v) = read(&mut b) {
        out.push(v);
    }
    out
}

fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    if b.len() < n {
        return None;
    }
    let (a, r) = b.split_at(n);
    *b = r;
    Some(a)
}

fn string(b: &mut &[u8]) -> Option<String> {
    let n = u16::from_be_bytes(take(b, 2)?.try_into().ok()?) as usize;
    Some(String::from_utf8_lossy(take(b, n)?).into_owned())
}

fn fields(b: &mut &[u8]) -> Option<Vec<(String, Amf)>> {
    let mut out = vec![];
    loop {
        let k = string(b)?;
        if k.is_empty() && b.first() == Some(&9) {
            *b = &b[1..];
            return Some(out);
        }
        out.push((k, read(b)?));
    }
}

pub fn read(b: &mut &[u8]) -> Option<Amf> {
    let t = *take(b, 1)?.first()?;
    Some(match t {
        0 => Amf::Number(f64::from_be_bytes(take(b, 8)?.try_into().ok()?)),
        1 => Amf::Bool(take(b, 1)?[0] != 0),
        2 => Amf::Str(string(b)?),
        3 => Amf::Object(fields(b)?),
        5 | 6 => Amf::Null,
        8 => {
            take(b, 4)?;
            Amf::Object(fields(b)?)
        }
        10 => {
            let n = u32::from_be_bytes(take(b, 4)?.try_into().ok()?);
            let mut items = vec![];
            for _ in 0..n {
                items.push(read(b)?);
            }
            Amf::Array(items)
        }
        11 => {
            take(b, 10)?;
            Amf::Null
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_roundtrip() {
        let v = obj(&[("app", s("live")), ("n", Amf::Number(3.0)), ("ok", Amf::Bool(true))]);
        let mut out = vec![];
        write(&mut out, &s("connect"));
        write(&mut out, &Amf::Number(1.0));
        write(&mut out, &v);
        write(&mut out, &Amf::Null);
        let all = read_all(&out);
        assert_eq!(all, vec![s("connect"), Amf::Number(1.0), v, Amf::Null]);
        assert_eq!(all[2].to_json()["app"], "live");
    }
}
