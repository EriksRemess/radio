use std::io::{self, Read};

pub const MP3_SAMPLE_RATES: [u32; 3] = [44100, 48000, 32000];
pub const MP3_BITRATES: [u32; 14] = [
    32000, 40000, 48000, 56000, 64000, 80000, 96000, 112000, 128000, 160000, 192000, 224000,
    256000, 320000,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Aac,
    Mp3,
    Opus,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Self::Aac => "AAC",
            Self::Mp3 => "MP3",
            Self::Opus => "Ogg/Opus",
        }
    }
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Aac => "audio/aac",
            Self::Mp3 => "audio/mpeg",
            Self::Opus => "audio/ogg",
        }
    }
    pub fn read_frame(self, reader: &mut impl Read) -> io::Result<Vec<u8>> {
        match self {
            Self::Aac => crate::audio::read_frame(reader),
            Self::Mp3 => read_mp3(reader),
            Self::Opus => read_ogg(reader),
        }
    }
    pub fn read_headers(self, reader: &mut impl Read) -> io::Result<Vec<Vec<u8>>> {
        if self != Self::Opus {
            return Ok(vec![read_id3(reader)?]);
        }
        let head = read_ogg(reader)?;
        let tags = read_ogg(reader)?;
        let payload = &head[27 + head[26] as usize..];
        if head[5] != 2
            || head[18..22] != 0_u32.to_le_bytes()
            || payload.len() != 19
            || !payload.starts_with(b"OpusHead")
            || payload[8] != 1
            || payload[18] != 0
            || tags[5] != 0
            || tags[18..22] != 1_u32.to_le_bytes()
            || head[14..18] != tags[14..18]
            || !tags[27 + tags[26] as usize..].starts_with(b"OpusTags")
        {
            return Err(invalid("invalid Ogg/Opus headers"));
        }
        Ok(vec![head, tags])
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

// FFmpeg writes a leading ID3v2 tag for AAC and MP3. Cache it separately from
// audio so every listener receives the title, including after buffer rollover.
fn read_id3(reader: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut header = [0; 10];
    reader.read_exact(&mut header)?;
    if &header[..3] != b"ID3"
        || ![3, 4].contains(&header[3])
        || header[4] != 0
        || header[5] != 0
        || header[6..].iter().any(|v| v & 0x80 != 0)
    {
        return Err(invalid("invalid ID3v2 header"));
    }
    let size = header[6..]
        .iter()
        .fold(0_usize, |size, byte| (size << 7) | *byte as usize);
    if size > 8192 {
        return Err(invalid("ID3v2 tag exceeds 8 KiB"));
    }
    let mut tag = vec![0; 10 + size];
    tag[..10].copy_from_slice(&header);
    reader.read_exact(&mut tag[10..])?;
    Ok(tag)
}

// MPEG-1 Layer III only: the encoder is configured for 32, 44.1 or 48 kHz.
// Disable LAME's bit reservoir so a new listener can decode its very first frame.
fn read_mp3(reader: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut h = [0; 4];
    reader.read_exact(&mut h)?;
    let rate_index = (h[2] >> 2) & 3;
    let bitrate_index = h[2] >> 4;
    if h[0] != 255
        || h[1] & 0xfe != 0xfa
        || rate_index == 3
        || bitrate_index == 0
        || bitrate_index == 15
    {
        return Err(invalid("invalid MPEG-1 Layer III frame"));
    }
    let rate = MP3_SAMPLE_RATES[rate_index as usize];
    let bitrate = MP3_BITRATES[bitrate_index as usize - 1];
    let size = (144 * bitrate / rate) as usize + ((h[2] >> 1) & 1) as usize;
    let mut frame = vec![0; size];
    frame[..4].copy_from_slice(&h);
    reader.read_exact(&mut frame[4..])?;
    Ok(frame)
}

// Our FFmpeg output uses short pages with complete 20 ms Opus packets. Reject
// continued packets and oversized pages rather than publishing unsafe join points.
fn read_ogg(reader: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut head = [0; 27];
    reader.read_exact(&mut head)?;
    if &head[..4] != b"OggS" || head[4] != 0 || head[5] & !6 != 0 || head[26] == 0 {
        return Err(invalid("invalid or continued Ogg page"));
    }
    let mut page = head.to_vec();
    page.resize(27 + head[26] as usize, 0);
    reader.read_exact(&mut page[27..])?;
    let length = page.len() + page[27..].iter().map(|v| *v as usize).sum::<usize>();
    if length > 8192 || page.last() == Some(&255) {
        return Err(invalid("oversized or incomplete Ogg page"));
    }
    let start = page.len();
    page.resize(length, 0);
    reader.read_exact(&mut page[start..])?;
    let expected = u32::from_le_bytes(page[22..26].try_into().unwrap());
    if checksum(&page) != expected {
        return Err(invalid("Ogg checksum mismatch"));
    }
    Ok(page)
}

// Ogg uses an unreflected CRC-32 with polynomial 0x04c11db7 and initial value 0.
fn checksum(page: &[u8]) -> u32 {
    let mut crc = 0_u32;
    for (index, &byte) in page.iter().enumerate() {
        let byte = if (22..26).contains(&index) { 0 } else { byte };
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = (crc << 1) ^ if crc & 0x80000000 != 0 { 0x04c11db7 } else { 0 };
        }
    }
    crc
}

pub fn ogg_header(page: &[u8], cropped: bool) -> Vec<u8> {
    let mut page = page.to_vec();
    if cropped && page[5] & 2 != 0 {
        // RFC 7845 section 4.2 recommends 80 ms of pre-roll when cropping Opus.
        let offset = 27 + page[26] as usize + 10;
        page[offset..offset + 2].copy_from_slice(&3840_u16.to_le_bytes());
        let crc = checksum(&page);
        page[22..26].copy_from_slice(&crc.to_le_bytes());
    }
    page
}

pub fn ogg_audio(page: &[u8], sequence: &mut u32) -> Vec<u8> {
    let mut page = page.to_vec();
    page[18..22].copy_from_slice(&sequence.to_le_bytes());
    *sequence = sequence.wrapping_add(1);
    // Keep absolute granules: RFC 7845 section 4.5 explicitly supports joining
    // live streams this way. Only page numbers and checksums need rewriting.
    let crc = checksum(&page);
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    page
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id3_tag_is_bounded_and_does_not_consume_audio() {
        let mut data = b"ID3\x04\0\0\0\0\0\x03abcAUDIO".as_slice();
        assert_eq!(read_id3(&mut data).unwrap(), b"ID3\x04\0\0\0\0\0\x03abc");
        assert_eq!(data, b"AUDIO");
        assert!(read_id3(&mut &b"ID3\x04\0\0\0\0\0\x03ab"[..]).is_err());
        assert!(read_id3(&mut &b"ID3\x04\0\0\x7f\x7f\x7f\x7f"[..]).is_err());
        assert!(read_id3(&mut &b"ID3\x04\0\0\0\0\0\x80"[..]).is_err());
    }

    #[test]
    fn mp3_frames_are_complete_and_invalid_headers_fail() {
        let mut input = vec![0; 576];
        input[..4].copy_from_slice(&[0xff, 0xfb, 0xb4, 0]);
        assert_eq!(read_mp3(&mut &input[..]).unwrap(), input);
        assert!(read_mp3(&mut &input[..575]).is_err());
        input[2] = 0xfc;
        assert!(read_mp3(&mut &input[..]).is_err());
    }

    #[test]
    fn ogg_crc_and_sequence_are_rewritten_for_late_join() {
        let mut page = vec![0; 31];
        page[..4].copy_from_slice(b"OggS");
        page[26] = 1;
        page[27] = 3;
        page[28..].copy_from_slice(&[0xf8, 1, 2]);
        page[18..22].copy_from_slice(&900_u32.to_le_bytes());
        let crc = checksum(&page);
        page[22..26].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(read_ogg(&mut &page[..]).unwrap(), page);
        let mut sequence = 2;
        let rewritten = ogg_audio(&page, &mut sequence);
        assert_eq!(sequence, 3);
        assert_eq!(rewritten[18..22], 2_u32.to_le_bytes());
        assert_eq!(read_ogg(&mut &rewritten[..]).unwrap(), rewritten);
        page[30] ^= 1;
        assert!(read_ogg(&mut &page[..]).is_err());
    }
}
