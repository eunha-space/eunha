//! Just enough DER (X.690) to read certificates and the structures
//! attestation formats put inside them.
//!
//! Every value keeps the bytes it was read from, so a signature is checked
//! over exactly what was signed, as OpenSSL does, rather than over a
//! re-encoding.

use super::{invalid, Result};

pub const CLASS_UNIVERSAL: u8 = 0;
pub const CLASS_CONTEXT: u8 = 2;

pub const BOOLEAN: u32 = 1;
pub const INTEGER: u32 = 2;
pub const BIT_STRING: u32 = 3;
pub const OCTET_STRING: u32 = 4;
pub const OID: u32 = 6;
pub const SEQUENCE: u32 = 16;
pub const SET: u32 = 17;
pub const UTC_TIME: u32 = 23;
pub const GENERALIZED_TIME: u32 = 24;

/// One tag-length-value.
#[derive(Debug, Clone, Copy)]
pub struct Tlv<'a> {
    pub class: u8,
    pub constructed: bool,
    pub tag: u32,
    /// The contents.
    pub value: &'a [u8],
    /// The whole encoding, header included.
    pub raw: &'a [u8],
}

impl<'a> Tlv<'a> {
    /// The single value `bytes` holds; trailing bytes are refused, as
    /// `OpenSSL::ASN1.decode` refuses them.
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let mut rest = bytes;
        let tlv = read(&mut rest)?;
        if !rest.is_empty() {
            return invalid("DER has trailing bytes");
        }
        Ok(tlv)
    }

    pub fn is(&self, class: u8, tag: u32) -> bool {
        self.class == class && self.tag == tag
    }

    pub fn is_universal(&self, tag: u32) -> bool {
        self.is(CLASS_UNIVERSAL, tag)
    }

    /// The values inside a constructed one.
    pub fn children(&self) -> Result<Vec<Tlv<'a>>> {
        if !self.constructed {
            return invalid("DER value is not constructed");
        }
        let mut rest = self.value;
        let mut out = Vec::new();
        while !rest.is_empty() {
            out.push(read(&mut rest)?);
        }
        Ok(out)
    }

    /// The children of a SEQUENCE.
    pub fn sequence(&self) -> Result<Vec<Tlv<'a>>> {
        if !self.is_universal(SEQUENCE) {
            return invalid("DER value is not a SEQUENCE");
        }
        self.children()
    }

    pub fn octets(&self) -> Result<&'a [u8]> {
        if !self.is_universal(OCTET_STRING) || self.constructed {
            return invalid("DER value is not an OCTET STRING");
        }
        Ok(self.value)
    }

    /// The bits of a BIT STRING with no unused bits.
    pub fn bits(&self) -> Result<&'a [u8]> {
        if !self.is_universal(BIT_STRING) || self.constructed {
            return invalid("DER value is not a BIT STRING");
        }
        match self.value.split_first() {
            Some((0, bits)) => Ok(bits),
            _ => invalid("BIT STRING has unused bits"),
        }
    }

    pub fn oid(&self) -> Result<&'a [u8]> {
        if !self.is_universal(OID) || self.constructed {
            return invalid("DER value is not an OBJECT IDENTIFIER");
        }
        Ok(self.value)
    }

    pub fn boolean(&self) -> Result<bool> {
        if !self.is_universal(BOOLEAN) || self.value.len() != 1 {
            return invalid("DER value is not a BOOLEAN");
        }
        Ok(self.value[0] != 0)
    }

    /// A non-negative INTEGER small enough for a `u64`.
    pub fn small_uint(&self) -> Result<u64> {
        if !self.is_universal(INTEGER) && !self.is_universal(10) {
            return invalid("DER value is not an INTEGER");
        }
        let bytes = self.value;
        if bytes.is_empty() || bytes[0] & 0x80 != 0 {
            return invalid("INTEGER is negative or empty");
        }
        let bytes = match bytes {
            [0, rest @ ..] if !rest.is_empty() => rest,
            _ => bytes,
        };
        if bytes.len() > 8 {
            return invalid("INTEGER is too large");
        }
        Ok(bytes.iter().fold(0u64, |n, b| (n << 8) | u64::from(*b)))
    }

    /// The magnitude of a non-negative INTEGER, leading zeroes removed.
    pub fn unsigned_bytes(&self) -> Result<&'a [u8]> {
        if !self.is_universal(INTEGER) || self.value.is_empty() || self.value[0] & 0x80 != 0 {
            return invalid("DER value is not a non-negative INTEGER");
        }
        let mut bytes = self.value;
        while bytes.len() > 1 && bytes[0] == 0 {
            bytes = &bytes[1..];
        }
        Ok(bytes)
    }
}

/// Read one value off the front of `input`.
pub fn read<'a>(input: &mut &'a [u8]) -> Result<Tlv<'a>> {
    let bytes = *input;
    let first = *bytes
        .first()
        .ok_or(super::Error::Invalid("DER ends early"))?;
    let class = first >> 6;
    let constructed = first & 0x20 != 0;
    let mut pos = 1;
    let mut tag = u32::from(first & 0x1f);
    if tag == 0x1f {
        // High tag number form: base 128, high bit set on all but the last.
        tag = 0;
        loop {
            let b = *bytes
                .get(pos)
                .ok_or(super::Error::Invalid("DER ends early"))?;
            pos += 1;
            if tag > (u32::MAX >> 7) {
                return invalid("DER tag is too large");
            }
            tag = (tag << 7) | u32::from(b & 0x7f);
            if b & 0x80 == 0 {
                break;
            }
        }
    }
    let len_byte = *bytes
        .get(pos)
        .ok_or(super::Error::Invalid("DER ends early"))?;
    pos += 1;
    let len = if len_byte < 0x80 {
        usize::from(len_byte)
    } else {
        let count = usize::from(len_byte & 0x7f);
        if count == 0 || count > 4 {
            return invalid("unsupported DER length");
        }
        let mut len = 0usize;
        for _ in 0..count {
            let b = *bytes
                .get(pos)
                .ok_or(super::Error::Invalid("DER ends early"))?;
            pos += 1;
            len = (len << 8) | usize::from(b);
        }
        len
    };
    let end = pos
        .checked_add(len)
        .filter(|end| *end <= bytes.len())
        .ok_or(super::Error::Invalid("DER length overruns the data"))?;
    let tlv = Tlv {
        class,
        constructed,
        tag,
        value: &bytes[pos..end],
        raw: &bytes[..end],
    };
    *input = &bytes[end..];
    Ok(tlv)
}

/// An OBJECT IDENTIFIER's contents, from its dotted form, for comparisons.
pub fn oid(dotted: &str) -> Vec<u8> {
    let arcs: Vec<u64> = dotted.split('.').map(|a| a.parse().unwrap()).collect();
    let mut out = Vec::new();
    let mut push = |mut n: u64| {
        let mut chunk = vec![(n & 0x7f) as u8];
        n >>= 7;
        while n > 0 {
            chunk.push(0x80 | (n & 0x7f) as u8);
            n >>= 7;
        }
        chunk.reverse();
        out.extend(chunk);
    };
    push(arcs[0] * 40 + arcs[1]);
    for arc in &arcs[2..] {
        push(*arc);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_high_tag_numbers_and_long_lengths() {
        // [702] EXPLICIT INTEGER 0, as Android's authorization lists use.
        let bytes = [0xbf, 0x85, 0x3e, 0x03, 0x02, 0x01, 0x00];
        let tlv = Tlv::parse(&bytes).unwrap();
        assert_eq!(
            (tlv.class, tlv.tag, tlv.constructed),
            (CLASS_CONTEXT, 702, true)
        );
        assert_eq!(tlv.children().unwrap()[0].small_uint().unwrap(), 0);

        let mut long = vec![0x04, 0x82, 0x01, 0x00];
        long.extend_from_slice(&[7; 256]);
        assert_eq!(Tlv::parse(&long).unwrap().octets().unwrap().len(), 256);
        assert!(Tlv::parse(&long[..200]).is_err());
        assert_eq!(
            oid("1.2.840.10045.2.1"),
            [0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01]
        );
    }
}
