//! Page-level Ogg validation used by crash recovery: walks the pages,
//! verifies each CRC and finds where the last complete page ends.

use std::io;
use std::path::Path;

const HEADER_LEN: usize = 27;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScanResult {
    /// Byte length covered by complete, CRC-valid pages.
    pub valid_len: u64,
    pub file_len: u64,
    pub pages: u64,
    /// Granule position of the last valid page that completes a packet.
    pub last_granule: Option<u64>,
    /// Opus pre-skip from the OpusHead packet, if present and valid.
    pub preskip: Option<u16>,
    /// The last valid page carries the end-of-stream flag.
    pub eos: bool,
}

impl ScanResult {
    /// Playable duration of an Opus part in milliseconds.
    pub fn duration_ms(&self) -> u64 {
        match (self.last_granule, self.preskip) {
            (Some(g), Some(p)) => g.saturating_sub(p as u64) / 48,
            _ => 0,
        }
    }
}

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut r = (i as u32) << 24;
        let mut j = 0;
        while j < 8 {
            r = if r & 0x8000_0000 != 0 {
                (r << 1) ^ 0x04c1_1db7
            } else {
                r << 1
            };
            j += 1;
        }
        table[i] = r;
        i += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = crc_table();

/// Ogg CRC-32 (polynomial 0x04c11db7, no reflection, zero init and xor-out).
pub fn ogg_crc(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |crc, &b| {
        (crc << 8) ^ CRC_TABLE[((crc >> 24) as u8 ^ b) as usize]
    })
}

pub fn scan_file(path: &Path) -> io::Result<ScanResult> {
    Ok(scan_bytes(&std::fs::read(path)?))
}

pub fn scan_bytes(data: &[u8]) -> ScanResult {
    let mut r = ScanResult {
        file_len: data.len() as u64,
        ..Default::default()
    };
    let mut off = 0usize;
    while off + HEADER_LEN <= data.len() {
        let h = &data[off..];
        if &h[..4] != b"OggS" || h[4] != 0 {
            break;
        }
        let nsegs = h[26] as usize;
        if off + HEADER_LEN + nsegs > data.len() {
            break;
        }
        let lacing = &h[HEADER_LEN..HEADER_LEN + nsegs];
        let body_len: usize = lacing.iter().map(|&l| l as usize).sum();
        let page_len = HEADER_LEN + nsegs + body_len;
        if off + page_len > data.len() {
            break;
        }
        let page = &data[off..off + page_len];
        let stored = u32::from_le_bytes([page[22], page[23], page[24], page[25]]);
        let mut check = page.to_vec();
        check[22..26].fill(0);
        if ogg_crc(&check) != stored {
            break;
        }
        let body = &page[HEADER_LEN + nsegs..];
        if r.pages == 0 && body.len() >= 19 && &body[..8] == b"OpusHead" {
            r.preskip = Some(u16::from_le_bytes([body[10], body[11]]));
        }
        let granule = u64::from_le_bytes(page[6..14].try_into().expect("8 bytes"));
        if granule != u64::MAX {
            r.last_granule = Some(granule);
        }
        r.eos = page[5] & 0x04 != 0;
        r.pages += 1;
        off += page_len;
        r.valid_len = off as u64;
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pages produced by the `ogg` crate (independent CRC implementation) must
    /// validate; a page cut short must not.
    #[test]
    fn truncated_page_is_excluded() {
        let dir = std::env::temp_dir().join(format!("mr-scan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.ogg");
        let mut w = crate::opus_ogg::PartWriter::create(
            &path,
            7,
            &Default::default(),
            crate::opus_ogg::Layout::Split,
            &[],
        )
        .unwrap();
        for _ in 0..120 {
            w.write_frame(&[0.1; 640]).unwrap();
        }
        drop(w); // simulated crash: no finish(); the unfinished third page is never written
        let full = std::fs::read(&path).unwrap();
        let cut = &full[..full.len() - 7];
        let r = scan_bytes(cut);
        assert!(r.valid_len < cut.len() as u64);
        // Head, tags and the first one-second audio page survive; the second
        // page was cut.
        assert_eq!(r.pages, 3);
        assert_eq!(r.preskip, Some(312));
        // 1 s of packets minus the 6.5 ms pre-skip.
        assert_eq!(r.duration_ms(), (48_000 - 312) / 48);
        std::fs::remove_dir_all(dir).ok();
    }
}
