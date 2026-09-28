//! Subset of SSRI: parse, hash and verify `<algorithm>-<base64>` strings.

use crate::error::{FlashnpmError, ErrorCode};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use sha1::Digest as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Algorithm {
    Sha512,
    Sha384,
    Sha256,
    Sha1,
}

impl Algorithm {
    fn rank(self) -> u8 {
        match self {
            Self::Sha512 => 0,
            Self::Sha384 => 1,
            Self::Sha256 => 2,
            Self::Sha1 => 3,
        }
    }

    fn byte_len(self) -> usize {
        match self {
            Self::Sha512 => 64,
            Self::Sha384 => 48,
            Self::Sha256 => 32,
            Self::Sha1 => 20,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Sha512 => "sha512",
            Self::Sha384 => "sha384",
            Self::Sha256 => "sha256",
            Self::Sha1 => "sha1",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "sha512" => Some(Self::Sha512),
            "sha384" => Some(Self::Sha384),
            "sha256" => Some(Self::Sha256),
            "sha1" => Some(Self::Sha1),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedIntegrity {
    pub algorithm: Algorithm,
    /// Canonical base64 digest.
    pub digest: String,
}

fn fail(msg: impl Into<String>) -> FlashnpmError {
    FlashnpmError::new(ErrorCode::Eintegrity, msg)
}

/// Parse `sha512-<base64> [sha1-<base64> ...]`, keeping the strongest.
/// Throws `EINTEGRITY` when nothing parses.
pub fn parse_integrity(value: &str) -> Result<ParsedIntegrity, FlashnpmError> {
    if value.trim().is_empty() {
        return Err(fail(format!(
            "Invalid integrity: expected a string, got {value:?}"
        )));
    }
    let mut best: Option<ParsedIntegrity> = None;
    for entry in value.trim().split_whitespace() {
        // strip `?opts` suffix
        let entry = entry.split('?').next().unwrap_or(entry);
        let Some((algo, b64)) = entry.split_once('-') else {
            continue;
        };
        let Some(algorithm) = Algorithm::parse(algo) else {
            continue;
        };
        // base64 charset check
        if !b64
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
        {
            continue;
        }
        let Ok(raw) = B64.decode(b64) else { continue };
        if raw.len() != algorithm.byte_len() {
            continue;
        }
        let parsed = ParsedIntegrity {
            algorithm,
            digest: B64.encode(&raw),
        };
        if best
            .as_ref()
            .map_or(true, |b| algorithm.rank() < b.algorithm.rank())
        {
            best = Some(parsed);
        }
    }
    best.ok_or_else(|| {
        fail(format!(
            "Invalid integrity: no supported algorithm in {value:?}"
        ))
    })
}

/// Convert legacy hex `dist.shasum` into `sha1-<base64>`.
pub fn from_shasum(shasum: &str) -> Result<String, FlashnpmError> {
    let s = shasum.trim();
    if s.len() != 40 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(fail(format!(
            "Invalid shasum: expected 40 hex characters, got {shasum:?}"
        )));
    }
    let raw = hex::decode(s).map_err(|e| fail(e.to_string()))?;
    Ok(format!("sha1-{}", B64.encode(raw)))
}

/// Hash bytes, returning `<algorithm>-<base64>`.
pub fn hash_of(data: &[u8], algorithm: Algorithm) -> String {
    let raw = match algorithm {
        Algorithm::Sha512 => {
            use sha2::Sha512;
            let mut h = Sha512::new();
            h.update(data);
            h.finalize().to_vec()
        }
        Algorithm::Sha384 => {
            use sha2::Sha384;
            let mut h = Sha384::new();
            h.update(data);
            h.finalize().to_vec()
        }
        Algorithm::Sha256 => {
            use sha2::Sha256;
            let mut h = Sha256::new();
            h.update(data);
            h.finalize().to_vec()
        }
        Algorithm::Sha1 => {
            let mut h = sha1::Sha1::new();
            h.update(data);
            h.finalize().to_vec()
        }
    };
    format!("{}-{}", algorithm.name(), B64.encode(raw))
}

pub fn hash_sha512(data: &[u8]) -> String {
    hash_of(data, Algorithm::Sha512)
}

/// Verify bytes against an integrity string.
pub fn verify_bytes(data: &[u8], expected: &str) -> Result<(), FlashnpmError> {
    let parsed = parse_integrity(expected)?;
    let actual = hash_of(data, parsed.algorithm);
    if actual != format!("{}-{}", parsed.algorithm.name(), parsed.digest) {
        return Err(fail(format!(
            "Integrity check failed: expected {}-{}, got {actual}",
            parsed.algorithm.name(),
            parsed.digest
        )));
    }
    Ok(())
}

/// Streaming verifier: feed chunks, then call `verify`.
pub struct Verifier {
    algorithm: Algorithm,
    expected: String,
    sha512: Option<sha2::Sha512>,
    sha384: Option<sha2::Sha384>,
    sha256: Option<sha2::Sha256>,
    sha1: Option<sha1::Sha1>,
    sealed: bool,
}

impl Verifier {
    pub fn new(expected: &str) -> Result<Self, FlashnpmError> {
        let parsed = parse_integrity(expected)?;
        Ok(Self {
            algorithm: parsed.algorithm,
            expected: parsed.digest,
            sha512: matches!(parsed.algorithm, Algorithm::Sha512).then(sha2::Sha512::new),
            sha384: matches!(parsed.algorithm, Algorithm::Sha384).then(sha2::Sha384::new),
            sha256: matches!(parsed.algorithm, Algorithm::Sha256).then(sha2::Sha256::new),
            sha1: matches!(parsed.algorithm, Algorithm::Sha1).then(sha1::Sha1::new),
            sealed: false,
        })
    }

    pub fn update(&mut self, chunk: &[u8]) -> Result<(), FlashnpmError> {
        if self.sealed {
            return Err(fail("Cannot update a verifier after verify()"));
        }
        use sha2::Digest as _;
        match self.algorithm {
            Algorithm::Sha512 => self.sha512.as_mut().unwrap().update(chunk),
            Algorithm::Sha384 => self.sha384.as_mut().unwrap().update(chunk),
            Algorithm::Sha256 => self.sha256.as_mut().unwrap().update(chunk),
            Algorithm::Sha1 => self.sha1.as_mut().unwrap().update(chunk),
        }
        Ok(())
    }

    pub fn verify(mut self) -> Result<(), FlashnpmError> {
        self.sealed = true;
        use sha2::Digest as _;
        let raw = match self.algorithm {
            Algorithm::Sha512 => self.sha512.unwrap().finalize().to_vec(),
            Algorithm::Sha384 => self.sha384.unwrap().finalize().to_vec(),
            Algorithm::Sha256 => self.sha256.unwrap().finalize().to_vec(),
            Algorithm::Sha1 => self.sha1.unwrap().finalize().to_vec(),
        };
        let actual = B64.encode(raw);
        if actual != self.expected {
            return Err(fail(format!(
                "Integrity check failed: expected {}-{}, got {}-{actual}",
                self.algorithm.name(),
                self.expected,
                self.algorithm.name()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let h = hash_sha512(b"hello");
        assert!(h.starts_with("sha512-"));
        verify_bytes(b"hello", &h).unwrap();
        assert!(verify_bytes(b"bye", &h).is_err());
    }

    #[test]
    fn picks_strongest() {
        let weak = hash_of(b"x", Algorithm::Sha1);
        let strong = hash_of(b"x", Algorithm::Sha512);
        let p = parse_integrity(&format!("{weak} {strong}")).unwrap();
        assert_eq!(p.algorithm, Algorithm::Sha512);
    }

    #[test]
    fn shasum_converts() {
        let s = from_shasum("da39a3ee5e6b4b0d3255bfef95601890afd80709").unwrap();
        assert!(s.starts_with("sha1-"));
    }
}
