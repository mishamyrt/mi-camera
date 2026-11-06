//! Annex B parsing and RFC 7798 HEVC payload packetization.

/// Extract NAL units with at least a two-byte header, excluding Annex B prefixes.
pub fn nals(data: &[u8]) -> Vec<&[u8]> {
    let mut result = Vec::new();
    let mut begin = None;
    let mut i = 0;
    while i + 3 <= data.len() {
        let prefix = if data[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        if let Some(start) = begin {
            if i >= start + 2 {
                result.push(&data[start..i]);
            }
        }
        i += prefix;
        begin = Some(i);
    }
    if let Some(start) = begin {
        if data.len() >= start + 2 {
            result.push(&data[start..]);
        }
    }
    result
}

/// Read a NAL unit's type. Panics if `nal` is empty.
pub fn nal_type(nal: &[u8]) -> u8 {
    (nal[0] >> 1) & 63
}

/// Send a NAL unit as RFC 7798 payloads without decoding or encoding.
/// The callback receives each payload and its RTP marker flag.
pub fn packetize<E>(
    nal: &[u8],
    last_nal: bool,
    mut send: impl FnMut(&[u8], bool) -> Result<(), E>,
) -> Result<(), E> {
    const MAX_PAYLOAD: usize = 1200;
    if nal.len() <= MAX_PAYLOAD {
        return send(nal, last_nal);
    }
    let chunks = nal[2..].chunks(MAX_PAYLOAD - 3);
    let count = chunks.len();
    for (i, chunk) in chunks.enumerate() {
        let mut payload = [0; MAX_PAYLOAD];
        payload[0] = (nal[0] & 0x81) | (49 << 1);
        payload[1] = nal[1];
        payload[2] =
            nal_type(nal) | if i == 0 { 0x80 } else { 0 } | if i + 1 == count { 0x40 } else { 0 };
        payload[3..3 + chunk.len()].copy_from_slice(chunk);
        send(&payload[..3 + chunk.len()], last_nal && i + 1 == count)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b_mixed_prefixes() {
        let data = [0, 0, 0, 1, 64, 1, 3, 0, 0, 1, 66, 1, 4];
        assert_eq!(nals(&data), vec![&[64, 1, 3][..], &[66, 1, 4][..]]);
        assert!(nals(&[0, 0, 1, 64]).is_empty());
        assert!(nals(&[]).is_empty());
    }

    #[test]
    fn fragment_round_trip_and_markers() {
        let mut nal = vec![0x27, 0x09];
        nal.extend((0..5000).map(|i| (i % 251) as u8));
        let mut packets = Vec::new();
        packetize(&nal, true, |p, marker| {
            packets.push((p.to_vec(), marker));
            Ok::<_, ()>(())
        })
        .unwrap();
        let mut reconstructed = vec![
            (packets[0].0[0] & 0x81) | ((packets[0].0[2] & 63) << 1),
            packets[0].0[1],
        ];
        for (i, (p, marker)) in packets.iter().enumerate() {
            assert!(p.len() <= 1200);
            assert_eq!(p[2] & 0x80 != 0, i == 0);
            assert_eq!(p[2] & 0x40 != 0, i + 1 == packets.len());
            assert_eq!(*marker, i + 1 == packets.len());
            reconstructed.extend_from_slice(&p[3..]);
        }
        assert_eq!(reconstructed, nal);
        let mut single = Vec::new();
        packetize(&nal[..2], false, |p, m| {
            single.push((p.to_vec(), m));
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(single, vec![(nal[..2].to_vec(), false)]);
    }
}
