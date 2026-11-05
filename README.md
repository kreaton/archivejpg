# archivejpg

A command-line tool that copies JPEG photos from a messy folder tree into a tidy archive organised by capture date, and records where every photo came from.

```text
archive/
├── manifest-20261105162620.csv
└── 2004/
    └── 09/
        └── 25/
            ├── a.jpg
            └── a.jpg.xmp
```

## Why

Photo collections tend to spread across old drives, phone backups and folders named by event, often with the same photo in several places. `archivejpg` brings them together in one `YYYY/MM/DD` structure without losing track of their origin:

- **Source files are never changed.** Photos are only read and copied.
- **Runs are repeatable.** Photos that are already archived are skipped, so you can run it again whenever new photos arrive.
- **Nothing is overwritten.** If a different photo already has the same name, the new one is stored under a new name.
- **Every run is recorded.** A CSV manifest lists what happened to each photo.

## Build

Requires Rust 1.85 or newer (edition 2024).

```bash
cargo build --release
```

The binary is written to `target/release/archivejpg` (`archivejpg.exe` on Windows).

## Usage

```text
archivejpg [OPTIONS] <SOURCE> <TARGET>
```

| Argument / option | Description |
| --- | --- |
| `<SOURCE>` | Directory to scan recursively for `.jpg` and `.jpeg` files |
| `<TARGET>` | Root directory of the archive, created if it doesn't exist |
| `--xmpembed` | Embed the origin as XMP in the copied JPEG |
| `--exifembed` | Embed the origin in the EXIF `UserComment` of the copied JPEG |
| `--xmpsidecar` | Write the origin to a sidecar file next to the copy (for example `<name>.jpg.xmp`) |
| `-h`, `--help` | Print help |
| `-V`, `--version` | Print version |

The embedding options can be combined. Without them, each copy is byte-identical to the original.

```bash
archivejpg "D:\Unsorted" "E:\Archive"
```

```bash
archivejpg "D:\Unsorted" "E:\Archive" --xmpembed --xmpsidecar
```

### Example output

```text
copied    a.jpg -> 2004\09\25\a.jpg
duplicate sub\a-copy.jpg -> 2004\09\25\a.jpg (archived from a.jpg)

Done: 1 copied, 0 copied with new name, 0 skipped (already archived), 1 duplicates, 0 failed
Manifest: E:\Archive\manifest-20261105162620.csv
```

## How it works

### Which files are copied

Files with the extension `.jpg` or `.jpeg`, in any letter case. The copy keeps the original's file name and extension. Other formats, such as HEIC or PNG, are ignored. If the archive is inside the source folder, it is left out of the scan.

### Where a photo goes

The folder comes from the EXIF `DateTimeOriginal` date, then `DateTime`. If the photo has neither, the file's modified date is used.

The copy keeps the original's modified date. Its created date is the time it was archived.

### What happens to each photo

| Status | Meaning |
| --- | --- |
| `copied` | Archived under its original name |
| `renamed` | Archived as `<name>_<uuid>.<ext>`, because a different file already had that name |
| `skipped` | Already archived from the same relative source path, or already in the archive with an unknown origin (noted in the manifest) |
| `duplicate` | Already archived from a different source path, so not copied again |
| `failed` | Could not be processed; the reason is in the manifest's `note` column |

A photo counts as already archived when its SHA-256 hash and size match an archived photo **and** the modified dates are within 2 seconds of each other. The same bytes with a different modified date count as a new photo.

Previously archived photos are recognised from the earlier manifests in the target root. For files in the photo's date folder that no manifest covers, the embedded or sidecar origin is used, or failing that, the file's own hash and size.

### The manifest

Each run creates a new `manifest-YYYYMMDDHHMMSS.csv` in the target root, even if nothing was copied. Rows are written as they happen, so an interrupted run still leaves an accurate record. Keep these files: they are how later runs recognise what is already archived.

| Column | Content |
| --- | --- |
| `status` | One of the statuses above |
| `source_root` | Absolute path of the source directory |
| `source_path` | Path of the photo, relative to the source root |
| `target_path` | Path of the archived copy, relative to the target root |
| `existing_source_path` | For `skipped` and `duplicate`: where the archived copy came from |
| `size` | Size of the original in bytes |
| `modified` | Modified date of the original |
| `photo_date` | Date used for the archive folder |
| `sha256` | SHA-256 hash of the original |
| `metadata` | Embedding methods that were applied |
| `note` | Details, such as why an embedding was not applied |

### Origin metadata

The options store the source path, the original's hash and size, and the source root and archive time, so the origin can travel with the photo:

| Option | Stored in | Contents |
| --- | --- | --- |
| `--xmpembed` | XMP in the JPEG | Namespace `http://ns.archivejpg/1.0/`, plus `dc:source` unless one already exists. Existing XMP is kept. |
| `--exifembed` | EXIF `UserComment` | `archivejpg\|source=<path>\|sha256=<hash>\|size=<bytes>` (no source root or archive time) |
| `--xmpsidecar` | `<file name>.xmp`, such as `a.jpg.xmp` | Same XMP as `--xmpembed`, in the naming convention used by darktable and digiKam |

Embedding is careful with your photos:

- The copy is checked before it is written. If the image data would change or the EXIF date would be lost, that embedding is dropped.
- An existing `UserComment` that wasn't written by `archivejpg` is never overwritten.
- A failed embedding doesn't stop the photo from being archived. The reason appears in the `note` column.

Embedding changes the archived file, so its hash no longer matches the one in the manifest. That hash always describes the original.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | All photos were handled |
| `1` | At least one photo failed, or the run could not start (for example, the source doesn't exist) |
| `2` | Invalid arguments |

## Using it with exifsync

[exifsync](https://github.com/kreaton/exifsync) corrects file dates from EXIF data. The order matters, because `archivejpg` compares modified dates to recognise photos it has already archived:

1. Run `exifsync --update` on the **source**.
2. Run `archivejpg`.
3. If you also want correct created dates in the archive, run `exifsync --update` on the **archive**.

Don't run `exifsync --update` on the archive alone. If that changes the modified dates so they no longer match the source, the next `archivejpg` run treats those photos as new and stores them again under `<name>_<uuid>.<ext>`.

## Tests

```bash
cargo test
```

The tests generate their own minimal JPEG, so they need no sample photos.
