//! Framing of the transfer protocol. All integers are big-endian.
//!
//! ```text
//! client hello   "LMFT" | version u8 | token [32]
//! server reply   status u8 [| error]
//! request        op u8 (MANIFEST | FILE root u32, components, offset u64)
//! response       status u8 | manifest            (MANIFEST)
//!                status u8 | size u64 | frame*   (FILE)
//! frame          DATA len u32, bytes | END | ERROR error
//! error          kind u8 | path string | detail string
//! ```

use std::io::{self, Read, Write};

use crate::RemoteErrorKind;

pub(crate) const MAGIC: &[u8; 4] = b"LMFT";
pub(crate) const VERSION: u8 = 1;

pub(crate) const OP_MANIFEST: u8 = 1;
pub(crate) const OP_FILE: u8 = 2;

pub(crate) const STATUS_OK: u8 = 0;
pub(crate) const STATUS_ERROR: u8 = 1;

pub(crate) const FRAME_DATA: u8 = 0;
pub(crate) const FRAME_END: u8 = 1;
pub(crate) const FRAME_ERROR: u8 = 2;

/// Largest data frame; also the server's read buffer size.
pub(crate) const MAX_FRAME: usize = 256 * 1024;
const MAX_STRING: usize = 64 * 1024;
const MAX_COMPONENTS: usize = 4096;

pub(crate) fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

pub(crate) fn write_u8(w: &mut impl Write, value: u8) -> io::Result<()> {
    w.write_all(&[value])
}

pub(crate) fn write_u16(w: &mut impl Write, value: u16) -> io::Result<()> {
    w.write_all(&value.to_be_bytes())
}

pub(crate) fn write_u32(w: &mut impl Write, value: u32) -> io::Result<()> {
    w.write_all(&value.to_be_bytes())
}

pub(crate) fn write_u64(w: &mut impl Write, value: u64) -> io::Result<()> {
    w.write_all(&value.to_be_bytes())
}

pub(crate) fn write_i64(w: &mut impl Write, value: i64) -> io::Result<()> {
    w.write_all(&value.to_be_bytes())
}

pub(crate) fn read_u8(r: &mut impl Read) -> io::Result<u8> {
    let mut buf = [0u8; 1];
    r.read_exact(&mut buf)?;
    Ok(buf[0])
}

pub(crate) fn read_u16(r: &mut impl Read) -> io::Result<u16> {
    let mut buf = [0u8; 2];
    r.read_exact(&mut buf)?;
    Ok(u16::from_be_bytes(buf))
}

pub(crate) fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_be_bytes(buf))
}

pub(crate) fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_be_bytes(buf))
}

pub(crate) fn read_i64(r: &mut impl Read) -> io::Result<i64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(i64::from_be_bytes(buf))
}

pub(crate) fn write_str(w: &mut impl Write, value: &str) -> io::Result<()> {
    let bytes = value.as_bytes();
    let bytes = &bytes[..floor_char_boundary(value, MAX_STRING)];
    write_u32(w, bytes.len() as u32)?;
    w.write_all(bytes)
}

pub(crate) fn read_str(r: &mut impl Read) -> io::Result<String> {
    let len = read_u32(r)? as usize;
    if len > MAX_STRING {
        return Err(invalid("string too long"));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|_| invalid("string is not UTF-8"))
}

/// Path components below a copied root. Validation of their content is the
/// server's job ([`crate::manifest::resolve`]); here only sizes are bounded.
pub(crate) fn write_components(w: &mut impl Write, components: &[String]) -> io::Result<()> {
    if components.len() > MAX_COMPONENTS {
        return Err(invalid("path too deep"));
    }
    write_u16(w, components.len() as u16)?;
    for component in components {
        write_str(w, component)?;
    }
    Ok(())
}

pub(crate) fn read_components(r: &mut impl Read) -> io::Result<Vec<String>> {
    let count = read_u16(r)? as usize;
    if count > MAX_COMPONENTS {
        return Err(invalid("path too deep"));
    }
    (0..count).map(|_| read_str(r)).collect()
}

pub(crate) fn write_error(
    w: &mut impl Write,
    kind: RemoteErrorKind,
    path: &str,
    detail: &str,
) -> io::Result<()> {
    write_u8(w, kind.to_u8())?;
    write_str(w, path)?;
    write_str(w, detail)
}

pub(crate) fn read_error(r: &mut impl Read) -> io::Result<(RemoteErrorKind, String, String)> {
    let kind = RemoteErrorKind::from_u8(read_u8(r)?);
    let path = read_str(r)?;
    let detail = read_str(r)?;
    Ok((kind, path, detail))
}

fn floor_char_boundary(value: &str, max: usize) -> usize {
    if value.len() <= max {
        return value.len();
    }
    (0..=max)
        .rev()
        .find(|&index| value.is_char_boundary(index))
        .unwrap_or(0)
}
