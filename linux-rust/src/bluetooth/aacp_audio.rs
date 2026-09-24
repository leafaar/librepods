//! AACP framing for the AirPods proprietary hi-res microphone stream
//! (AAC-ELD @ 64 kHz, message_type 0x58 over L2CAP PSM 0x1001).

/// type-0x58 audio SDU layout: 22-byte header, then N x [ts:u32 LE][len:u8][au].
const TYPE58_HEADER_LEN: usize = 22;

#[inline]
fn u16le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

/// AACP packet header shared by every message: 04 00 04 00.
const AACP_HEADER: [u8; 4] = [0x04, 0x00, 0x04, 0x00];

/// Predicate for 0x58 *audio* frames (subtype 0x0001)
#[inline]
pub fn is_audio(sdu: &[u8]) -> bool {
    sdu.len() >= 8 && sdu[..4] == AACP_HEADER && u16le(sdu, 4) == 0x58 && u16le(sdu, 6) == 0x0001
}

/// Walk the sub-frames of one 0x58 audio SDU, invoking `emit` per AAC-ELD AU.
/// Returns the number of AUs emitted
pub fn demux_type58(sdu: &[u8], mut emit: impl FnMut(&[u8])) -> usize {
    let mut off = TYPE58_HEADER_LEN;
    let mut n = 0;

    while off + 5 <= sdu.len() {
        let au_len = sdu[off + 4] as usize;
        let start = off + 5;
        let end = start + au_len;

        if end > sdu.len() {
            break;
        }
        emit(&sdu[start..end]);
        n += 1;
        off = end;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Vec<u8> {
        let mut sdu = vec![0x04, 0x00, 0x04, 0x00, 0x58, 0x00, 0x01, 0x00];
        sdu.resize(TYPE58_HEADER_LEN, 0);
        sdu
    }

    fn push_au(sdu: &mut Vec<u8>, ts: u32, au: &[u8]) {
        sdu.extend_from_slice(&ts.to_le_bytes());
        sdu.push(u8::try_from(au.len()).unwrap());
        sdu.extend_from_slice(au);
    }

    fn collect(sdu: &[u8]) -> (usize, Vec<Vec<u8>>) {
        let mut aus = Vec::new();
        let n = demux_type58(sdu, |au| aus.push(au.to_vec()));
        (n, aus)
    }

    #[test]
    fn is_audio_accepts_full_header() {
        assert!(is_audio(&header()));
        assert!(is_audio(&header()[..8]));
    }

    #[test]
    fn is_audio_rejects_other_headers_and_short_input() {
        for i in 0..4 {
            let mut sdu = header();
            sdu[i] ^= 0x01;
            assert!(!is_audio(&sdu), "byte {i} flipped");
        }
        let mut other_type = header();
        other_type[4] = 0x57;
        assert!(!is_audio(&other_type));
        let mut other_subtype = header();
        other_subtype[6] = 0x02;
        assert!(!is_audio(&other_subtype));
        for len in 0..8 {
            assert!(!is_audio(&header()[..len]), "len {len}");
        }
    }

    #[test]
    fn demux_short_or_header_only_input_emits_nothing() {
        let sdu = header();
        for len in 0..=sdu.len() {
            assert_eq!(collect(&sdu[..len]).0, 0, "len {len}");
        }
        // Sub-frame header cut short: fewer than 5 bytes after the SDU header.
        let mut sdu = header();
        sdu.extend_from_slice(&[1, 2, 3, 4]);
        assert_eq!(collect(&sdu).0, 0);
    }

    #[test]
    fn demux_multiple_aus() {
        let mut sdu = header();
        push_au(&mut sdu, 1, &[0xAA; 3]);
        push_au(&mut sdu, 2, &[0xBB]);
        push_au(&mut sdu, 3, &[0xCC; 255]);
        let (n, aus) = collect(&sdu);
        assert_eq!(n, 3);
        assert_eq!(aus, vec![vec![0xAA; 3], vec![0xBB], vec![0xCC; 255]]);
    }

    #[test]
    fn demux_zero_length_au_is_emitted_and_walk_continues() {
        let mut sdu = header();
        push_au(&mut sdu, 1, &[]);
        push_au(&mut sdu, 2, &[0x11, 0x22]);
        let (n, aus) = collect(&sdu);
        assert_eq!(n, 2);
        assert_eq!(aus, vec![vec![], vec![0x11, 0x22]]);
    }

    #[test]
    fn demux_stops_at_truncated_au() {
        let mut sdu = header();
        push_au(&mut sdu, 1, &[0x01, 0x02]);
        push_au(&mut sdu, 2, &[0x03; 10]);
        // Drop the last byte of the second AU.
        sdu.pop();
        let (n, aus) = collect(&sdu);
        assert_eq!(n, 1);
        assert_eq!(aus, vec![vec![0x01, 0x02]]);

        // Every prefix of a valid SDU is handled without panicking and never
        // emits a partial AU.
        let mut full = header();
        push_au(&mut full, 1, &[0x05; 7]);
        push_au(&mut full, 2, &[0x06; 4]);
        for len in 0..=full.len() {
            let (n, aus) = collect(&full[..len]);
            let expect = match len {
                l if l >= full.len() => 2,
                l if l >= TYPE58_HEADER_LEN + 5 + 7 => 1,
                _ => 0,
            };
            assert_eq!(n, expect, "len {len}");
            assert!(aus.iter().all(|au| au.len() == 7 || au.len() == 4));
        }
    }
}
