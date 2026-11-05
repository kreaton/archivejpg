//! `archive-log.csv` in the target root: one row for every file copied, with
//! its file details and EXIF data. The file grows across runs.

use std::fs::{File, OpenOptions};
use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use exif::{Exif, In, Tag, Value};

pub const FILE_NAME: &str = "archive-log.csv";

const FILE_COLUMNS: [&str; 12] = [
    "copied_at",
    "status",
    "source_root",
    "source_path",
    "target_path",
    "size",
    "target_size",
    "source_created",
    "source_modified",
    "sha256",
    "target_sha256",
    "embedded_metadata",
];

const EXIF_COLUMNS: [(&str, Tag); 20] = [
    ("exif_date_time_original", Tag::DateTimeOriginal),
    ("exif_date_time_digitized", Tag::DateTimeDigitized),
    ("exif_date_time", Tag::DateTime),
    ("exif_offset_time_original", Tag::OffsetTimeOriginal),
    ("exif_make", Tag::Make),
    ("exif_model", Tag::Model),
    ("exif_lens_model", Tag::LensModel),
    ("exif_software", Tag::Software),
    ("exif_orientation", Tag::Orientation),
    ("exif_width", Tag::PixelXDimension),
    ("exif_height", Tag::PixelYDimension),
    ("exif_exposure_time", Tag::ExposureTime),
    ("exif_f_number", Tag::FNumber),
    ("exif_iso", Tag::PhotographicSensitivity),
    ("exif_focal_length", Tag::FocalLength),
    ("exif_flash", Tag::Flash),
    ("exif_artist", Tag::Artist),
    ("exif_copyright", Tag::Copyright),
    ("exif_image_description", Tag::ImageDescription),
    ("exif_gps_altitude", Tag::GPSAltitude),
];

const DATE_TAGS: [Tag; 3] = [Tag::DateTime, Tag::DateTimeOriginal, Tag::DateTimeDigitized];

/// Binary values longer than this, such as maker notes, are left out of `exif_all`.
const MAX_BINARY_LEN: usize = 64;

/// The file details of one copied photo.
pub struct FileDetails<'a> {
    pub copied_at: &'a str,
    pub status: &'a str,
    pub source_root: &'a str,
    pub source_path: &'a str,
    pub target_path: &'a str,
    pub size: u64,
    pub target_size: u64,
    pub source_created: &'a str,
    pub source_modified: &'a str,
    pub sha256: &'a str,
    pub target_sha256: &'a str,
    pub embedded_metadata: &'a str,
}

pub struct CopyLog {
    writer: csv::Writer<File>,
    pub path: PathBuf,
}

impl CopyLog {
    /// Opens the log for appending, and writes the header if the file is new.
    pub fn open(target_root: &Path) -> Result<Self> {
        let path = target_root.join(FILE_NAME);
        let file = OpenOptions::new().create(true).append(true).open(&path).with_context(|| {
            format!("cannot open {}; is it open in another program?", path.display())
        })?;
        let is_new = file.metadata()?.len() == 0;
        let mut writer = csv::Writer::from_writer(file);
        if is_new {
            let exif = EXIF_COLUMNS.iter().map(|(name, _)| *name);
            let header = FILE_COLUMNS
                .iter()
                .copied()
                .chain(exif)
                .chain(["exif_gps_latitude", "exif_gps_longitude", "exif_all"]);
            writer.write_record(header)?;
            writer.flush()?;
        }
        Ok(Self { writer, path })
    }

    /// `data` is the original photo, whose EXIF data is what gets logged. Rows
    /// are flushed immediately so an interrupted run still leaves an accurate log.
    pub fn write(&mut self, file: &FileDetails, data: &[u8]) -> Result<()> {
        let (size, target_size) = (file.size.to_string(), file.target_size.to_string());
        let mut record: Vec<String> = [
            file.copied_at,
            file.status,
            file.source_root,
            file.source_path,
            file.target_path,
            &size,
            &target_size,
            file.source_created,
            file.source_modified,
            file.sha256,
            file.target_sha256,
            file.embedded_metadata,
        ]
        .map(String::from)
        .to_vec();
        record.extend(exif_values(data));
        self.writer.write_record(&record)?;
        self.writer.flush()?;
        Ok(())
    }
}

/// One value for each EXIF column of the header. All empty for a photo
/// without readable EXIF data.
fn exif_values(data: &[u8]) -> Vec<String> {
    let Ok(exif) = exif::Reader::new().read_from_container(&mut Cursor::new(data)) else {
        return vec![String::new(); EXIF_COLUMNS.len() + 3];
    };
    let mut values: Vec<String> = EXIF_COLUMNS
        .iter()
        .map(|(_, tag)| exif.get_field(*tag, In::PRIMARY).map_or_else(String::new, display))
        .collect();
    values.push(gps_decimal(&exif, Tag::GPSLatitude, Tag::GPSLatitudeRef, "S"));
    values.push(gps_decimal(&exif, Tag::GPSLongitude, Tag::GPSLongitudeRef, "W"));
    values.push(all_fields(&exif));
    values
}

/// Text fields come from the raw bytes, because the library's display form
/// quotes and escapes them (`Søren` would become `"S\xc3\xb8ren"`). Dates do
/// use that form, as it is the `YYYY-MM-DD HH:MM:SS` the manifest has.
fn display(field: &exif::Field) -> String {
    match &field.value {
        Value::Ascii(parts) if !DATE_TAGS.contains(&field.tag) => parts
            .iter()
            .map(|bytes| String::from_utf8_lossy(bytes).trim_matches(['\0', ' ']).to_string())
            .collect::<Vec<_>>()
            .join(", "),
        _ => field.display_value().to_string(),
    }
}

/// Degrees as a signed decimal number, which maps and spreadsheets understand.
fn gps_decimal(exif: &Exif, value: Tag, reference: Tag, negative: &str) -> String {
    let Some(field) = exif.get_field(value, In::PRIMARY) else { return String::new() };
    let Value::Rational(parts) = &field.value else { return String::new() };
    let [degrees, minutes, seconds] = parts.as_slice() else { return String::new() };
    let decimal = degrees.to_f64() + minutes.to_f64() / 60.0 + seconds.to_f64() / 3600.0;
    if !decimal.is_finite() {
        return String::new();
    }
    let is_negative = exif.get_field(reference, In::PRIMARY).is_some_and(|f| display(f) == negative);
    format!("{:.6}", if is_negative { -decimal } else { decimal })
}

/// Every EXIF field of the photo itself (not its thumbnail), as `Tag=value; ...`.
fn all_fields(exif: &Exif) -> String {
    let mut fields: Vec<String> = exif
        .fields()
        .filter(|f| f.ifd_num == In::PRIMARY)
        .filter(|f| !matches!(&f.value, Value::Undefined(bytes, _) if bytes.len() > MAX_BINARY_LEN))
        .map(|f| format!("{}={}", f.tag, display(f)))
        .collect();
    fields.sort();
    fields.join("; ")
}
