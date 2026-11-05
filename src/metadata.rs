//! Origin metadata: embedding into / reading back from JPEG XMP, EXIF
//! UserComment and XMP sidecar files.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use little_exif::endian::Endian;
use little_exif::exif_tag::ExifTag;
use little_exif::filetype::FileExtension;
use little_exif::metadata::Metadata;

/// Where an archived file came from, plus the identity of the original
/// bytes (needed because embedding changes the archived file's hash/size).
pub struct Origin {
    pub source_root: String,
    pub source_path: String,
    pub sha256: String,
    pub size: u64,
    pub archived_at: String,
}

/// Origin information read back from an archived file.
pub struct StoredOrigin {
    pub source_path: String,
    pub sha256: String,
    pub size: u64,
}

const NS: &str = "http://ns.archivejpg/1.0/";
const BLOCK_START: &str = r#"<rdf:Description rdf:about="" xmlns:archivejpg="#;
const XMP_SIG: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const COMMENT_PREFIX: &str = "archivejpg|";
const APP1: u8 = 0xE1;
const SOS: u8 = 0xDA;

// ---------------------------------------------------------------- XMP text

fn xmp_block(o: &Origin, include_dc_source: bool) -> String {
    let (dc_ns, dc) = if include_dc_source {
        (
            r#" xmlns:dc="http://purl.org/dc/elements/1.1/""#,
            format!("\n   <dc:source>{}</dc:source>", esc(&o.source_path)),
        )
    } else {
        ("", String::new())
    };
    format!(
        r#"{BLOCK_START}"{NS}"{dc_ns}>{dc}
   <archivejpg:SourcePath>{}</archivejpg:SourcePath>
   <archivejpg:SourceRoot>{}</archivejpg:SourceRoot>
   <archivejpg:OriginalSHA256>{}</archivejpg:OriginalSHA256>
   <archivejpg:OriginalSize>{}</archivejpg:OriginalSize>
   <archivejpg:ArchivedAt>{}</archivejpg:ArchivedAt>
  </rdf:Description>
"#,
        esc(&o.source_path),
        esc(&o.source_root),
        o.sha256,
        o.size,
        esc(&o.archived_at),
    )
}

fn xmp_packet(o: &Origin) -> String {
    format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
         <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n \
         <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n  \
         {} </rdf:RDF>\n\
         </x:xmpmeta>\n\
         <?xpacket end=\"w\"?>",
        xmp_block(o, true)
    )
}

/// Add our block to an existing XMP packet, replacing a previous one of ours.
fn merge_xmp(existing: &str, o: &Origin) -> Result<String> {
    let mut s = existing.to_string();
    if let Some(start) = s.find(BLOCK_START) {
        if let Some(len) = s[start..].find("</rdf:Description>") {
            s.replace_range(start..start + len + "</rdf:Description>".len(), "");
        }
    }
    let block = xmp_block(o, !s.contains("dc:source"));
    let pos = s.rfind("</rdf:RDF>").context("existing XMP has no </rdf:RDF>")?;
    s.insert_str(pos, &block);
    Ok(s)
}

fn parse_xmp(xmp: &str) -> Option<StoredOrigin> {
    Some(StoredOrigin {
        source_path: xmp_field(xmp, "SourcePath")?,
        sha256: xmp_field(xmp, "OriginalSHA256")?,
        size: xmp_field(xmp, "OriginalSize")?.parse().ok()?,
    })
}

/// Reads `archivejpg:<name>` in element or attribute form (other tools
/// may rewrite the packet into either).
fn xmp_field(xmp: &str, name: &str) -> Option<String> {
    let open = format!("<archivejpg:{name}>");
    if let Some(i) = xmp.rfind(&open) {
        let rest = &xmp[i + open.len()..];
        let j = rest.find('<')?;
        return Some(unesc(&rest[..j]));
    }
    let attr = format!("archivejpg:{name}=\"");
    let i = xmp.rfind(&attr)?;
    let rest = &xmp[i + attr.len()..];
    Some(unesc(&rest[..rest.find('"')?]))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn unesc(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"")
        .replace("&apos;", "'").replace("&amp;", "&")
}

// ------------------------------------------------------- JPEG segments

struct Segment {
    marker: u8,
    start: usize, // offset of the 0xFF
    end: usize,   // exclusive
}

impl Segment {
    fn payload<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        &data[self.start + 4..self.end]
    }
}

/// Parses the header segments up to the start of scan. Returns them and the
/// offset of the SOS marker; everything from there on is image data.
fn header_segments(data: &[u8]) -> Result<(Vec<Segment>, usize)> {
    ensure!(data.starts_with(&[0xFF, 0xD8]), "not a JPEG file");
    let mut segs = Vec::new();
    let mut pos = 2;
    loop {
        ensure!(pos + 1 < data.len(), "truncated JPEG header");
        ensure!(data[pos] == 0xFF, "corrupt JPEG header at offset {pos}");
        let marker = data[pos + 1];
        if marker == 0xFF {
            pos += 1; // fill byte
            continue;
        }
        if marker == SOS {
            return Ok((segs, pos));
        }
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            pos += 2; // no length field
            continue;
        }
        ensure!(pos + 3 < data.len(), "truncated JPEG header");
        let len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        let end = pos + 2 + len;
        ensure!(len >= 2 && end <= data.len(), "corrupt JPEG segment length");
        segs.push(Segment { marker, start: pos, end });
        pos = end;
    }
}

fn is_xmp(seg: &Segment, data: &[u8]) -> bool {
    seg.marker == APP1 && seg.payload(data).starts_with(XMP_SIG)
}

/// Offset where the image data begins (used to verify embedding never
/// touches it).
pub fn image_data_offset(data: &[u8]) -> Result<usize> {
    Ok(header_segments(data)?.1)
}

// ------------------------------------------------------- embedded XMP

pub fn embed_xmp(data: &[u8], o: &Origin) -> Result<Vec<u8>> {
    let (segs, _) = header_segments(data)?;
    let existing = segs.iter().find(|s| is_xmp(s, data));

    let packet = match existing {
        Some(seg) => {
            let text = std::str::from_utf8(&seg.payload(data)[XMP_SIG.len()..])
                .context("existing XMP is not UTF-8")?;
            merge_xmp(text, o)?
        }
        None => xmp_packet(o),
    };

    let seg_len = 2 + XMP_SIG.len() + packet.len();
    if seg_len > 0xFFFF {
        bail!("XMP packet would exceed the 64 KB JPEG segment limit");
    }
    let mut seg = Vec::with_capacity(seg_len + 2);
    seg.extend_from_slice(&[0xFF, APP1]);
    seg.extend_from_slice(&(seg_len as u16).to_be_bytes());
    seg.extend_from_slice(XMP_SIG);
    seg.extend_from_slice(packet.as_bytes());

    // Replace the existing XMP segment, or insert after the leading
    // APP0 (JFIF) / APP1 (Exif) segments.
    let (cut_start, cut_end) = match existing {
        Some(s) => (s.start, s.end),
        None => {
            let at = segs.iter()
                .take_while(|s| s.marker == 0xE0 || s.marker == APP1)
                .last()
                .map_or(2, |s| s.end);
            (at, at)
        }
    };
    let mut out = Vec::with_capacity(data.len() + seg.len());
    out.extend_from_slice(&data[..cut_start]);
    out.extend_from_slice(&seg);
    out.extend_from_slice(&data[cut_end..]);
    Ok(out)
}

fn read_embedded_xmp(data: &[u8]) -> Option<StoredOrigin> {
    let (segs, _) = header_segments(data).ok()?;
    segs.iter()
        .filter(|s| is_xmp(s, data))
        .find_map(|s| parse_xmp(&String::from_utf8_lossy(&s.payload(data)[XMP_SIG.len()..])))
}

// ---------------------------------------------------- EXIF UserComment

/// Returns the new file bytes, or None (with reason) when an unrelated
/// UserComment is already present and was left alone.
pub fn embed_exif(data: &[u8], o: &Origin) -> Result<std::result::Result<Vec<u8>, String>> {
    if let Some(existing) = read_user_comment(data) {
        let existing = existing.trim_matches(|c: char| c == '\0' || c.is_whitespace());
        if !existing.is_empty() && !existing.starts_with(COMMENT_PREFIX) {
            return Ok(Err("EXIF UserComment already in use; not overwritten".into()));
        }
    }

    let text = format!(
        "{COMMENT_PREFIX}source={}|sha256={}|size={}",
        o.source_path, o.sha256, o.size
    );
    let mut md = Metadata::new_from_vec(&data.to_vec(), FileExtension::JPEG)
        .context("cannot read EXIF")?;
    let mut value = Vec::new();
    if text.is_ascii() {
        value.extend_from_slice(b"ASCII\0\0\0");
        value.extend_from_slice(text.as_bytes());
    } else {
        value.extend_from_slice(b"UNICODE\0");
        let little = matches!(md.get_endian(), Endian::Little);
        for unit in text.encode_utf16() {
            value.extend_from_slice(&if little { unit.to_le_bytes() } else { unit.to_be_bytes() });
        }
    }
    md.set_tag(ExifTag::UserComment(value));

    let mut out = data.to_vec();
    md.write_to_vec(&mut out, FileExtension::JPEG).context("cannot write EXIF")?;
    Ok(Ok(out))
}

fn read_user_comment(data: &[u8]) -> Option<String> {
    let exif = exif::Reader::new().read_from_container(&mut Cursor::new(data)).ok()?;
    let field = exif.get_field(exif::Tag::UserComment, exif::In::PRIMARY)?;
    let exif::Value::Undefined(bytes, _) = &field.value else { return None };
    if bytes.len() < 8 {
        return None;
    }
    let (header, body) = bytes.split_at(8);
    if header == b"UNICODE\0" {
        let units: Vec<u16> = body
            .chunks_exact(2)
            .map(|c| if exif.little_endian() { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
            .collect();
        Some(String::from_utf16_lossy(&units))
    } else {
        Some(String::from_utf8_lossy(body).into_owned())
    }
}

fn read_exif_origin(data: &[u8]) -> Option<StoredOrigin> {
    let comment = read_user_comment(data)?;
    let rest = comment.trim_end_matches('\0').strip_prefix(COMMENT_PREFIX)?;
    let (mut source, mut sha, mut size) = (None, None, None);
    for part in rest.split('|') {
        match part.split_once('=') {
            Some(("source", v)) => source = Some(v.to_string()),
            Some(("sha256", v)) => sha = Some(v.to_string()),
            Some(("size", v)) => size = v.parse().ok(),
            _ => {}
        }
    }
    Some(StoredOrigin { source_path: source?, sha256: sha?, size: size? })
}

// ------------------------------------------------------------- sidecar

/// `IMG_0001.jpg` -> `IMG_0001.jpg.xmp` (the darktable / digiKam convention).
pub fn sidecar_path(jpg: &Path) -> PathBuf {
    let mut name = jpg.file_name().unwrap_or_default().to_os_string();
    name.push(".xmp");
    jpg.with_file_name(name)
}

pub fn write_sidecar(jpg: &Path, o: &Origin) -> Result<()> {
    let path = sidecar_path(jpg);
    fs::write(&path, xmp_packet(o)).with_context(|| format!("cannot write {}", path.display()))
}

// ------------------------------------------------------------- reading

/// Origin stored with an archived file by any of the three methods.
pub fn read_origin(jpg: &Path, data: &[u8]) -> Option<StoredOrigin> {
    read_embedded_xmp(data)
        .or_else(|| read_exif_origin(data))
        .or_else(|| parse_xmp(&fs::read_to_string(sidecar_path(jpg)).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal generated JPEG with an EXIF capture date. Its entropy data
    /// is not a decodable picture, which these tests don't need.
    fn sample() -> Vec<u8> {
        let mut jpeg = vec![0xFF, 0xD8];
        jpeg.extend_from_slice(b"\xFF\xE0\x00\x10JFIF\0\x01\x01\x00\x00\x01\x00\x01\x00\x00");
        jpeg.extend_from_slice(b"\xFF\xDA\x00\x08\x01\x01\x00\x00\x3F\x00");
        jpeg.extend_from_slice(&[0x12, 0x34, 0x56, 0x78, 0xFF, 0xD9]);

        let mut md = Metadata::new();
        md.set_tag(ExifTag::DateTimeOriginal("2004:09:25 11:28:37".to_string()));
        md.write_to_vec(&mut jpeg, FileExtension::JPEG).unwrap();
        jpeg
    }

    fn origin(source_path: &str) -> Origin {
        Origin {
            source_root: r"C:\photos".into(),
            source_path: source_path.into(),
            sha256: "ab12".into(),
            size: 42,
            archived_at: "2026-10-15T12:00:00+02:00".into(),
        }
    }

    fn assert_origin(o: Option<StoredOrigin>, source_path: &str) {
        let o = o.expect("origin not found");
        assert_eq!(o.source_path, source_path);
        assert_eq!(o.sha256, "ab12");
        assert_eq!(o.size, 42);
    }

    #[test]
    fn exif_round_trip_ascii_and_unicode() {
        let data = sample();
        for path in [r"Holiday\a.jpg", r"Ferie\Ærø & Søby\å.jpg"] {
            let out = embed_exif(&data, &origin(path)).unwrap().unwrap();
            assert_origin(read_exif_origin(&out), path);
        }
    }

    #[test]
    fn xmp_round_trip_and_replace() {
        let data = sample();
        let once = embed_xmp(&data, &origin("first.jpg")).unwrap();
        let twice = embed_xmp(&once, &origin(r"Ærø <&> \x.jpg")).unwrap();
        assert_origin(read_embedded_xmp(&twice), r"Ærø <&> \x.jpg");
        let text = String::from_utf8_lossy(&twice);
        assert_eq!(text.matches(BLOCK_START).count(), 1, "old block must be replaced");
        assert_eq!(text.matches("<dc:source>").count(), 1);
    }

    #[test]
    fn image_data_untouched() {
        let data = sample();
        let exif = embed_exif(&data, &origin("a.jpg")).unwrap().unwrap();
        let both = embed_xmp(&exif, &origin("a.jpg")).unwrap();
        let a = image_data_offset(&data).unwrap();
        let b = image_data_offset(&both).unwrap();
        assert_eq!(data[a..], both[b..]);
    }

    #[test]
    fn foreign_user_comment_is_kept() {
        let data = sample();
        let mut md = Metadata::new_from_vec(&data, FileExtension::JPEG).unwrap();
        md.set_tag(ExifTag::UserComment(b"ASCII\0\0\0A comment by someone else".to_vec()));
        let mut with_comment = data.clone();
        md.write_to_vec(&mut with_comment, FileExtension::JPEG).unwrap();
        assert!(embed_exif(&with_comment, &origin("a.jpg")).unwrap().is_err());
    }
}
