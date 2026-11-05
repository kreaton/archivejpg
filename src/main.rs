mod manifest;
mod metadata;

use std::collections::HashSet;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime};
use clap::Parser;
use filetime::FileTime;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use manifest::{Entry, Index, Manifest, Row};
use metadata::Origin;

/// Recursively copy .jpg and .jpeg files from a source directory into a
/// target/YYYY/MM/DD archive structure. Every run writes a
/// manifest-YYYYMMDDHHMMSS.csv in the target directory.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Directory to scan recursively for .jpg and .jpeg files
    source: PathBuf,
    /// Root directory of the archive
    target: PathBuf,
    /// Embed the origin (source path, original hash) as XMP in the copied JPEG
    #[arg(long)]
    xmpembed: bool,
    /// Embed the origin in the EXIF UserComment of the copied JPEG
    /// (left alone if the photo already has an unrelated UserComment)
    #[arg(long)]
    exifembed: bool,
    /// Write the origin to an XMP sidecar file next to the copy (e.g. <name>.jpg.xmp)
    #[arg(long)]
    xmpsidecar: bool,
}

#[derive(Default)]
struct Stats {
    copied: u32,
    renamed: u32,
    skipped: u32,
    duplicate: u32,
    failed: u32,
}

struct Archiver {
    args: Args,
    source: PathBuf,
    target: PathBuf,
    index: Index,
    /// Date folders already scanned into the index during this run.
    indexed_dirs: HashSet<PathBuf>,
    started: String,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if !args.source.is_dir() {
        bail!("source directory does not exist: {}", args.source.display());
    }
    fs::create_dir_all(&args.target)
        .with_context(|| format!("cannot create target directory {}", args.target.display()))?;

    let source = dunce::canonicalize(&args.source)?;
    let target = dunce::canonicalize(&args.target)?;
    let index = Index::load(&target)?;
    // A run within the same second as the previous one waits for a fresh name.
    let mut now = Local::now();
    while target.join(manifest_name(now)).exists() {
        std::thread::sleep(std::time::Duration::from_millis(250));
        now = Local::now();
    }
    let mut manifest = Manifest::create(target.join(manifest_name(now)))?;
    let mut archiver = Archiver {
        args,
        source: source.clone(),
        target: target.clone(),
        index,
        indexed_dirs: HashSet::new(),
        started: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
    };
    let mut stats = Stats::default();

    let walker = WalkDir::new(&source)
        .into_iter()
        // Don't re-archive the archive if target lives inside source.
        .filter_entry(|e| !e.path().starts_with(&target));

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("warning: {e}");
                stats.failed += 1;
                continue;
            }
        };
        if !entry.file_type().is_file() || !is_jpeg(entry.path()) {
            continue;
        }

        let src = entry.path();
        let row = archiver.process(src).unwrap_or_else(|e| {
            eprintln!("error     {}: {e:#}", src.display());
            Row {
                status: "failed",
                source_root: source.display().to_string(),
                source_path: rel_str(src, &source),
                note: format!("{e:#}"),
                ..Default::default()
            }
        });

        match row.status {
            "copied" => stats.copied += 1,
            "renamed" => stats.renamed += 1,
            "skipped" => stats.skipped += 1,
            "duplicate" => stats.duplicate += 1,
            _ => stats.failed += 1,
        }
        if row.status != "failed" {
            let mut line = format!("{:<9} {} -> {}", row.status, row.source_path, row.target_path);
            if row.status == "duplicate" {
                line += &format!(" (archived from {})", row.existing_source_path);
            }
            if !row.note.is_empty() {
                line += &format!(" [{}]", row.note);
            }
            println!("{line}");
        }
        manifest.write(&row)?;
    }

    println!(
        "\nDone: {} copied, {} copied with new name, {} skipped (already archived), \
         {} duplicates, {} failed",
        stats.copied, stats.renamed, stats.skipped, stats.duplicate, stats.failed
    );
    println!("Manifest: {}", manifest.path.display());
    if stats.failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

impl Archiver {
    fn process(&mut self, src: &Path) -> Result<Row> {
        let data = fs::read(src)?;
        let mtime = fs::metadata(src)?.modified()?;
        let size = data.len() as u64;
        let sha = sha256_hex(&data);
        let rel = rel_str(src, &self.source);
        let date = photo_date(&data).unwrap_or_else(|| DateTime::<Local>::from(mtime).date_naive());

        let mut row = Row {
            source_root: self.source.display().to_string(),
            source_path: rel.clone(),
            size: size.to_string(),
            modified: DateTime::<Local>::from(mtime).format("%Y-%m-%d %H:%M:%S").to_string(),
            photo_date: date.to_string(),
            sha256: sha.clone(),
            ..Default::default()
        };

        let dest_dir = self.target
            .join(format!("{:04}", date.year()))
            .join(format!("{:02}", date.month()))
            .join(format!("{:02}", date.day()));
        fs::create_dir_all(&dest_dir)?;
        let file_name = src.file_name().context("missing file name")?;
        let dest = dest_dir.join(file_name);
        let stem = src.file_stem().unwrap_or_default().to_string_lossy().into_owned();

        // Same photo already archived (same original size, hash and modified
        // time), under this name or any other?
        self.index_dir(&dest_dir);
        for target_rel in self.index.targets_with_hash(&sha).to_vec() {
            let candidate = self.target.join(&target_rel);
            let Some(entry) = self.index.get(&target_rel) else { continue };
            let Ok(meta) = fs::metadata(&candidate) else { continue };
            if entry.size != size || !same_time(meta.modified()?, mtime) {
                continue;
            }
            row.target_path = target_rel;
            row.status = match entry.source_path.clone() {
                Some(s) if !s.eq_ignore_ascii_case(&rel) => {
                    row.existing_source_path = s;
                    "duplicate"
                }
                Some(s) => {
                    row.existing_source_path = s;
                    "skipped"
                }
                None => {
                    row.note = "origin of archived copy unknown".into();
                    "skipped"
                }
            };
            return Ok(row);
        }

        let (final_dest, status) = if dest.exists() {
            let ext = src.extension().unwrap_or_default().to_string_lossy();
            (dest_dir.join(format!("{stem}_{}.{ext}", uuid::Uuid::new_v4())), "renamed")
        } else {
            (dest, "copied")
        };

        let origin = Origin {
            source_root: self.source.display().to_string(),
            source_path: rel.clone(),
            sha256: sha.clone(),
            size,
            archived_at: self.started.clone(),
        };
        let (bytes, applied, notes) = self.with_metadata(data, &origin);
        fs::write(&final_dest, &bytes)
            .with_context(|| format!("cannot write {}", final_dest.display()))?;
        filetime::set_file_mtime(&final_dest, FileTime::from_system_time(mtime))?;

        let mut applied = applied;
        let mut notes = notes;
        if self.args.xmpsidecar {
            match metadata::write_sidecar(&final_dest, &origin) {
                Ok(()) => applied.push("xmpsidecar"),
                Err(e) => notes.push(format!("sidecar failed: {e:#}")),
            }
        }

        let target_rel = rel_str(&final_dest, &self.target);
        self.index.add(&target_rel, Entry { sha256: sha, size, source_path: Some(rel) });
        row.status = status;
        row.target_path = target_rel;
        row.metadata = applied.join(";");
        row.note = notes.join("; ");
        Ok(row)
    }

    /// Applies the requested embeddings. A failing step is reported in the
    /// notes and skipped; the photo is still archived.
    fn with_metadata(&self, data: Vec<u8>, origin: &Origin) -> (Vec<u8>, Vec<&'static str>, Vec<String>) {
        let mut bytes = data;
        let mut applied = Vec::new();
        let mut notes = Vec::new();

        if self.args.exifembed {
            match metadata::embed_exif(&bytes, origin) {
                Ok(Ok(new)) => match verify(&bytes, &new) {
                    Ok(()) => {
                        bytes = new;
                        applied.push("exifembed");
                    }
                    Err(e) => notes.push(format!("exif not embedded: {e:#}")),
                },
                Ok(Err(reason)) => notes.push(reason),
                Err(e) => notes.push(format!("exif not embedded: {e:#}")),
            }
        }
        if self.args.xmpembed {
            match metadata::embed_xmp(&bytes, origin).and_then(|new| verify(&bytes, &new).map(|()| new)) {
                Ok(new) => {
                    bytes = new;
                    applied.push("xmpembed");
                }
                Err(e) => notes.push(format!("xmp not embedded: {e:#}")),
            }
        }
        (bytes, applied, notes)
    }

    /// The first time a date folder is visited, adds its archived photos that
    /// earlier manifests don't cover to the index. The original hash/size
    /// come from the file's own metadata when present, since embedding
    /// changes the file bytes.
    fn index_dir(&mut self, dir: &Path) {
        if !self.indexed_dirs.insert(dir.to_path_buf()) {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else { return };
        for path in entries.filter_map(|e| e.ok().map(|e| e.path())) {
            let target_rel = rel_str(&path, &self.target);
            if !path.is_file() || !is_jpeg(&path) || self.index.contains(&target_rel) {
                continue;
            }
            let data = match fs::read(&path) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("warning: cannot read {}: {e}", path.display());
                    continue;
                }
            };
            let entry = match metadata::read_origin(&path, &data) {
                Some(o) => Entry { sha256: o.sha256.to_lowercase(), size: o.size, source_path: Some(o.source_path) },
                None => Entry { sha256: sha256_hex(&data), size: data.len() as u64, source_path: None },
            };
            self.index.add(&target_rel, entry);
        }
    }
}

/// Embedding must leave the image data and capture date untouched.
fn verify(before: &[u8], after: &[u8]) -> Result<()> {
    let a = metadata::image_data_offset(before)?;
    let b = metadata::image_data_offset(after)?;
    ensure!(before[a..] == after[b..], "image data would change");
    ensure!(photo_date(before) == photo_date(after), "EXIF date would be lost");
    Ok(())
}

fn manifest_name(t: DateTime<Local>) -> String {
    t.format("manifest-%Y%m%d%H%M%S.csv").to_string()
}

fn rel_str(path: &Path, root: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

fn is_jpeg(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"))
}

/// Timestamps are compared with a 2 second tolerance to accommodate
/// filesystems with coarse resolution (FAT/exFAT).
fn same_time(a: SystemTime, b: SystemTime) -> bool {
    let diff = match a.duration_since(b) {
        Ok(d) => d,
        Err(e) => e.duration(),
    };
    diff.as_secs() < 2
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// Date the photo was taken, from EXIF DateTimeOriginal (or DateTime).
fn photo_date(data: &[u8]) -> Option<NaiveDate> {
    let exif = exif::Reader::new().read_from_container(&mut Cursor::new(data)).ok()?;
    [exif::Tag::DateTimeOriginal, exif::Tag::DateTime]
        .iter()
        .find_map(|tag| {
            let field = exif.get_field(*tag, exif::In::PRIMARY)?;
            let text = field.display_value().to_string();
            NaiveDateTime::parse_from_str(&text, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|dt| dt.date())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_extensions_match_in_any_case() {
        for name in ["a.jpg", "a.JPG", "a.Jpg", "a.jpeg", "a.JPEG", "a.JpEg"] {
            assert!(is_jpeg(Path::new(name)), "{name} should match");
        }
        for name in ["a.png", "a.heic", "a.jpe", "jpg", "a.jpg.xmp"] {
            assert!(!is_jpeg(Path::new(name)), "{name} should not match");
        }
    }
}
