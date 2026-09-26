//! Persistent flag for one bad lesson-manifest row.
//!
//! The flag lives in HedronDB state (or an in-memory stand-in for tests).
//! It is never written to `lattice.db` or the manifest file. There is no
//! scheduler and no notifier here: [`ManifestHook`] is the extension point.

use serde::Serialize;

use crate::error::{Error, Result};

/// What the reconciler should do with one malformed manifest row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookAction {
    /// Hold only the lanes this row could describe.
    Quarantine,
    /// Old total lockdown: every in-scope lane becomes `cannot_tell`.
    Abort,
}

/// One malformed manifest row, after date and lane sniffing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadRow {
    /// Inclusive 1-based line range of the raw row.
    pub row_span: [usize; 2],
    /// SHA-256 hex of the raw row bytes.
    pub row_hash: String,
    pub reason: String,
    pub date: Option<String>,
    /// Empty when the row's lane keys could not be recovered.
    pub lanes: Vec<String>,
}

/// Called once per in-scope (or undated) bad row. The default is quarantine.
///
/// A later post-cron chain can implement this to ping Fire Watch. This slice
/// ships the trait only; nothing here schedules or notifies.
pub trait ManifestHook {
    fn on_bad_row(&self, bad: &BadRow) -> HookAction {
        let _ = bad;
        HookAction::Quarantine
    }
}

/// Default hook. Quarantines the row instead of locking every lane.
#[derive(Debug, Default, Clone, Copy)]
pub struct QuarantineOnBadRow;

impl ManifestHook for QuarantineOnBadRow {}

/// `manifest_quarantine` row. `row_span` is stored so a later pass can tell
/// that this span's bytes changed and now parse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestQuarantine {
    pub row_hash: String,
    pub date: Option<String>,
    pub lanes: Vec<String>,
    pub reason: String,
    pub first_seen_at: String,
    pub last_seen_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleared_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acked_by: Option<String>,
    pub row_span: [usize; 2],
}

impl ManifestQuarantine {
    /// Still blocking lanes. Acked and auto-cleared flags are not active.
    pub fn is_active(&self) -> bool {
        self.cleared_at.is_none() && self.acked_by.is_none()
    }
}

/// Hedron-owned quarantine flags. Implementors must not open `lattice.db`.
pub trait QuarantineStore {
    fn get(&self, row_hash: &str) -> Result<Option<ManifestQuarantine>>;
    fn upsert(&mut self, flag: &ManifestQuarantine) -> Result<()>;
    fn list(&self) -> Result<Vec<ManifestQuarantine>>;
    /// Manual clear. Records who acked the row and stops it from holding lanes.
    fn ack(&mut self, row_hash: &str, acked_by: &str, at: &str) -> Result<()>;
}

/// Process-local flags. Tests use this; reconcile against a [`crate::Store`]
/// uses the sqlite table instead.
#[derive(Debug, Default, Clone)]
pub struct MemoryQuarantineStore {
    flags: Vec<ManifestQuarantine>,
}

impl MemoryQuarantineStore {
    pub fn new() -> Self {
        Self { flags: Vec::new() }
    }
}

impl QuarantineStore for MemoryQuarantineStore {
    fn get(&self, row_hash: &str) -> Result<Option<ManifestQuarantine>> {
        Ok(self
            .flags
            .iter()
            .find(|flag| flag.row_hash == row_hash)
            .cloned())
    }

    fn upsert(&mut self, flag: &ManifestQuarantine) -> Result<()> {
        if let Some(existing) = self
            .flags
            .iter_mut()
            .find(|item| item.row_hash == flag.row_hash)
        {
            *existing = flag.clone();
        } else {
            self.flags.push(flag.clone());
        }
        Ok(())
    }

    fn list(&self) -> Result<Vec<ManifestQuarantine>> {
        let mut flags = self.flags.clone();
        flags.sort_by(|left, right| {
            left.first_seen_at
                .cmp(&right.first_seen_at)
                .then_with(|| left.row_hash.cmp(&right.row_hash))
        });
        Ok(flags)
    }

    fn ack(&mut self, row_hash: &str, acked_by: &str, at: &str) -> Result<()> {
        let Some(flag) = self.flags.iter_mut().find(|item| item.row_hash == row_hash) else {
            return Err(Error::NotFound("manifest quarantine"));
        };
        flag.acked_by = Some(acked_by.to_string());
        flag.cleared_at = Some(at.to_string());
        Ok(())
    }
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = sha256(bytes);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h = [
        0x6a09e667_u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).saturating_mul(8);
    let mut msg = Vec::with_capacity(data.len() + 72);
    msg.extend_from_slice(data);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::sha256_hex;

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
