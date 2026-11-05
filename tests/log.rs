//! Runs the real binary on generated JPEGs and checks `archive-log.csv`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use little_exif::exif_tag::ExifTag;
use little_exif::filetype::FileExtension;
use little_exif::metadata::Metadata;
use little_exif::rational::uR64;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("archivejpg-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A minimal JPEG. Its entropy data is not a decodable picture, which
/// archivejpg doesn't need. `seed` makes the photos differ.
fn jpeg(seed: u8) -> Vec<u8> {
    let mut data = vec![0xFF, 0xD8];
    data.extend_from_slice(b"\xFF\xE0\x00\x10JFIF\0\x01\x01\x00\x00\x01\x00\x01\x00\x00");
    data.extend_from_slice(b"\xFF\xDA\x00\x08\x01\x01\x00\x00\x3F\x00");
    data.extend_from_slice(&[seed, 0x34, 0x56, 0x78, 0xFF, 0xD9]);
    data
}

fn rational(nominator: u32) -> uR64 {
    uR64 { nominator, denominator: 1 }
}

fn jpeg_with_exif(seed: u8) -> Vec<u8> {
    let mut data = jpeg(seed);
    let mut md = Metadata::new();
    md.set_tag(ExifTag::DateTimeOriginal("2004:09:25 11:28:37".to_string()));
    md.set_tag(ExifTag::Make("TestCam".to_string()));
    md.set_tag(ExifTag::Model("Model \"X\", 1".to_string()));
    md.set_tag(ExifTag::Artist("Søren".to_string()));
    md.set_tag(ExifTag::Orientation(vec![6]));
    md.set_tag(ExifTag::ISO(vec![200]));
    md.set_tag(ExifTag::FNumber(vec![uR64 { nominator: 28, denominator: 10 }]));
    md.set_tag(ExifTag::ExposureTime(vec![uR64 { nominator: 1, denominator: 125 }]));
    md.set_tag(ExifTag::FocalLength(vec![rational(50)]));
    md.set_tag(ExifTag::ExifImageWidth(vec![4032]));
    md.set_tag(ExifTag::GPSLatitudeRef("N".to_string()));
    md.set_tag(ExifTag::GPSLatitude(vec![rational(55), rational(30), rational(0)]));
    md.set_tag(ExifTag::GPSLongitudeRef("W".to_string()));
    md.set_tag(ExifTag::GPSLongitude(vec![rational(10), rational(15), rational(0)]));
    md.write_to_vec(&mut data, FileExtension::JPEG).unwrap();
    data
}

fn archive(source: &Path, target: &Path, extra: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_archivejpg"))
        .arg(source)
        .arg(target)
        .args(extra)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn read_log(target: &Path) -> Vec<HashMap<String, String>> {
    let mut reader = csv::Reader::from_path(target.join("archive-log.csv")).unwrap();
    let headers = reader.headers().unwrap().clone();
    reader
        .records()
        .map(|r| {
            let record = r.unwrap();
            assert_eq!(record.len(), headers.len(), "row and header differ in length");
            headers.iter().map(String::from).zip(record.iter().map(String::from)).collect()
        })
        .collect()
}

#[test]
fn logs_every_copied_file_once_with_file_and_exif_data() {
    let dir = TempDir::new();
    let (source, target) = (dir.0.join("src"), dir.0.join("archive"));
    fs::create_dir_all(source.join("Ærø")).unwrap();
    fs::write(source.join("Ærø").join("å.jpg"), jpeg_with_exif(1)).unwrap();
    fs::write(source.join("plain.JPG"), jpeg(2)).unwrap();

    let output = archive(&source, &target, &[]);
    assert!(output.contains("archive-log.csv"), "{output}");

    let rows = read_log(&target);
    assert_eq!(rows.len(), 2);
    let photo = rows.iter().find(|r| r["source_path"].ends_with("å.jpg")).unwrap();
    assert_eq!(photo["status"], "copied");
    assert_eq!(photo["target_path"], ["2004", "09", "25", "å.jpg"].join(std::path::MAIN_SEPARATOR_STR));
    assert_eq!(photo["size"], jpeg_with_exif(1).len().to_string());
    assert_eq!(photo["target_size"], photo["size"]);
    assert_eq!(photo["sha256"], photo["target_sha256"]);
    assert_eq!(photo["sha256"].len(), 64);
    assert!(!photo["source_modified"].is_empty() && !photo["copied_at"].is_empty());
    assert_eq!(photo["exif_date_time_original"], "2004-09-25 11:28:37");
    assert_eq!(photo["exif_make"], "TestCam");
    assert_eq!(photo["exif_model"], "Model \"X\", 1");
    assert_eq!(photo["exif_artist"], "Søren");
    assert_eq!(photo["exif_orientation"], "row 0 at right and column 0 at top");
    assert_eq!(photo["exif_iso"], "200");
    assert_eq!(photo["exif_f_number"], "2.8");
    assert_eq!(photo["exif_exposure_time"], "1/125");
    assert_eq!(photo["exif_focal_length"], "50");
    assert_eq!(photo["exif_width"], "4032");
    assert_eq!(photo["exif_gps_latitude"], "55.500000");
    assert_eq!(photo["exif_gps_longitude"], "-10.250000");
    assert!(photo["exif_all"].contains("Make=TestCam"), "{}", photo["exif_all"]);
    assert!(photo["exif_all"].contains("GPSLatitudeRef=N"), "{}", photo["exif_all"]);

    let plain = rows.iter().find(|r| r["source_path"] == "plain.JPG").unwrap();
    assert_eq!(plain["exif_make"], "");
    assert_eq!(plain["exif_all"], "");

    // A rerun copies nothing and logs nothing; a new photo adds one row.
    archive(&source, &target, &[]);
    assert_eq!(read_log(&target).len(), 2);

    fs::write(source.join("new.jpg"), jpeg(3)).unwrap();
    archive(&source, &target, &[]);
    assert_eq!(read_log(&target).len(), 3);

    let text = fs::read_to_string(target.join("archive-log.csv")).unwrap();
    assert_eq!(text.matches("copied_at").count(), 1, "header written more than once");
}

#[test]
fn renamed_copies_and_embedding_are_logged() {
    let dir = TempDir::new();
    let (first, second, target) = (dir.0.join("a"), dir.0.join("b"), dir.0.join("archive"));
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    fs::write(first.join("same.jpg"), jpeg_with_exif(1)).unwrap();
    fs::write(second.join("same.jpg"), jpeg_with_exif(2)).unwrap();

    archive(&first, &target, &[]);
    archive(&second, &target, &["--xmpembed"]);

    let rows = read_log(&target);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["status"], "renamed");
    assert_ne!(rows[1]["target_path"], rows[0]["target_path"]);
    assert_eq!(rows[1]["embedded_metadata"], "xmpembed");
    assert_ne!(rows[1]["sha256"], rows[1]["target_sha256"]);
    assert_ne!(rows[1]["size"], rows[1]["target_size"]);
    // What is logged describes the original, and the date survives embedding.
    assert_eq!(rows[1]["exif_date_time_original"], "2004-09-25 11:28:37");
}
