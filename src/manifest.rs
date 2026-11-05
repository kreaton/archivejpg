//! CSV manifest written for every run, and the index of previously archived
//! files rebuilt from earlier manifests.

use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const HEADER: [&str; 11] = [
    "status",
    "source_root",
    "source_path",
    "target_path",
    "existing_source_path",
    "size",
    "modified",
    "photo_date",
    "sha256",
    "metadata",
    "note",
];

#[derive(Default)]
pub struct Row {
    pub status: &'static str,
    pub source_root: String,
    pub source_path: String,
    pub target_path: String,
    /// For skipped/duplicate: where the already archived copy came from.
    pub existing_source_path: String,
    pub size: String,
    pub modified: String,
    pub photo_date: String,
    pub sha256: String,
    pub metadata: String,
    pub note: String,
}

pub struct Manifest {
    writer: csv::Writer<File>,
    pub path: PathBuf,
}

impl Manifest {
    pub fn create(path: PathBuf) -> Result<Self> {
        let file = File::create_new(&path)
            .with_context(|| format!("cannot create manifest {}", path.display()))?;
        let mut writer = csv::Writer::from_writer(file);
        writer.write_record(HEADER)?;
        writer.flush()?;
        Ok(Self { writer, path })
    }

    /// Rows are flushed immediately so an interrupted run still leaves an
    /// accurate manifest.
    pub fn write(&mut self, r: &Row) -> Result<()> {
        self.writer.write_record([
            r.status,
            &r.source_root,
            &r.source_path,
            &r.target_path,
            &r.existing_source_path,
            &r.size,
            &r.modified,
            &r.photo_date,
            &r.sha256,
            &r.metadata,
            &r.note,
        ])?;
        self.writer.flush()?;
        Ok(())
    }
}

/// An archived file as recorded in a manifest.
pub struct Entry {
    pub sha256: String,
    pub size: u64,
    /// None when the file was found in the archive without origin metadata.
    pub source_path: Option<String>,
}

/// Archived files keyed by target path (relative to the target root,
/// lower-cased) and by original content hash.
#[derive(Default)]
pub struct Index {
    by_target: HashMap<String, Entry>,
    by_hash: HashMap<String, Vec<String>>,
}

impl Index {
    pub fn add(&mut self, target_rel: &str, entry: Entry) {
        let key = target_rel.to_lowercase();
        if let Some(old) = self.by_target.get(&key) {
            if let Some(list) = self.by_hash.get_mut(&old.sha256) {
                list.retain(|t| t.to_lowercase() != key);
            }
        }
        self.by_hash.entry(entry.sha256.clone()).or_default().push(target_rel.to_string());
        self.by_target.insert(key, entry);
    }

    pub fn contains(&self, target_rel: &str) -> bool {
        self.by_target.contains_key(&target_rel.to_lowercase())
    }

    pub fn get(&self, target_rel: &str) -> Option<&Entry> {
        self.by_target.get(&target_rel.to_lowercase())
    }

    pub fn targets_with_hash(&self, sha256: &str) -> &[String] {
        self.by_hash.get(sha256).map_or(&[], Vec::as_slice)
    }

    /// Loads copied/renamed rows from every `manifest-*.csv` in the target root.
    pub fn load(target_root: &Path) -> Result<Self> {
        let mut index = Self::default();
        let mut manifests: Vec<PathBuf> = fs::read_dir(target_root)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.starts_with("manifest-") && n.to_ascii_lowercase().ends_with(".csv")
                })
            })
            .collect();
        manifests.sort();

        for path in manifests {
            if let Err(e) = index.load_one(&path) {
                eprintln!("warning: cannot read {}: {e:#}", path.display());
            }
        }
        Ok(index)
    }

    fn load_one(&mut self, path: &Path) -> Result<()> {
        let mut reader = csv::Reader::from_path(path)?;
        let headers = reader.headers()?.clone();
        let col = |name: &str| {
            headers.iter().position(|h| h == name)
                .with_context(|| format!("missing column {name}"))
        };
        let (status, target, source, sha, size) =
            (col("status")?, col("target_path")?, col("source_path")?, col("sha256")?, col("size")?);

        for record in reader.records() {
            let record = record?;
            if !matches!(record.get(status), Some("copied" | "renamed")) {
                continue;
            }
            let (Some(t), Some(src), Some(h), Some(Ok(sz))) = (
                record.get(target),
                record.get(source),
                record.get(sha),
                record.get(size).map(str::parse::<u64>),
            ) else {
                continue;
            };
            self.add(t, Entry { sha256: h.to_string(), size: sz, source_path: Some(src.to_string()) });
        }
        Ok(())
    }
}
