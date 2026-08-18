//! Reading disk images, compressed or not.
//!
//! Decompression happens here, in the unprivileged parent process, on the way
//! into the pipe that feeds `dd`. The privileged helper never learns the image
//! was compressed — it still just writes the bytes it is handed. That keeps the
//! whole feature out of the code that runs as root.

use anyhow::{anyhow, Context, Result};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// Compressed containers we recognise but cannot stream. Anything listed here
/// must be refused rather than treated as a raw image: writing a .gz to a
/// device byte for byte produces a drive that looks written and boots nothing.
const UNSUPPORTED_ARCHIVES: &[(&str, &str)] = &[
    ("gz", "gzip"),
    ("bz2", "bzip2"),
    ("zst", "zstd"),
    ("zip", "zip"),
    ("7z", "7-Zip"),
    ("lz4", "LZ4"),
    ("lzma", "LZMA"),
];

fn extension_of(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

fn is_xz(path: &str) -> bool {
    extension_of(path).as_deref() == Some("xz")
}

/// Refuse a compressed format we would otherwise write out verbatim.
fn reject_unsupported_archive(path: &str) -> Result<()> {
    let Some(ext) = extension_of(path) else {
        return Ok(());
    };
    if let Some((_, name)) = UNSUPPORTED_ARCHIVES.iter().find(|(e, _)| *e == ext) {
        return Err(anyhow!(
            "{name} images are not supported yet — decompress it first, or use a .xz image."
        ));
    }
    // A .tar.xz decompresses cleanly to a tarball, which we would then write to
    // the device and happily verify — a drive that passed every check and boots
    // nothing. Refuse the one archive-in-xz that people actually reach for.
    if ext == "xz" {
        let stem = Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if stem.to_ascii_lowercase().ends_with(".tar") {
            return Err(anyhow!(
                "that is a .tar.xz archive, not a disk image — extract it and pick the image inside."
            ));
        }
    }
    Ok(())
}

/// Open an image as a stream of the bytes that will land on the device.
///
/// For a plain image that is the file itself; for `.xz` it is the decompressed
/// stream. Callers see one `Read` either way.
pub fn open_image(path: &str) -> Result<Box<dyn Read>> {
    reject_unsupported_archive(path)?;
    let file = File::open(path).with_context(|| format!("opening {path}"))?;
    let buffered = BufReader::with_capacity(4 * 1024 * 1024, file);
    if is_xz(path) {
        Ok(Box::new(xz2::read::XzDecoder::new(buffered)))
    } else {
        Ok(Box::new(buffered))
    }
}

/// How many bytes this image will occupy on the device once written.
///
/// This is the number the drive-capacity check and the progress bar need, and
/// it must be known *before* writing. For a plain file it is the file length.
/// For `.xz` it is read from the stream index at the end of the file, so an
/// 8 GB image costs a seek rather than a full decompression pass.
pub fn image_size(path: &str) -> Result<u64> {
    reject_unsupported_archive(path)?;
    let mut file = File::open(path).with_context(|| format!("opening {path}"))?;
    let file_len = file.metadata()?.len();
    if !is_xz(path) {
        return Ok(file_len);
    }
    xz_uncompressed_size(&mut file, file_len)
        .with_context(|| format!("reading the xz index of {path}"))
}

/// Sum the uncompressed sizes recorded in an xz stream index.
///
/// Layout, from the end of the file (xz file format spec, §2.1.2 and §3):
///   Stream Footer: CRC32(4) | Backward Size(4) | Stream Flags(2) | "YZ"(2)
/// Backward Size stores `real_size / 4 - 1`, and the Index sits immediately
/// before the footer:
///   Index: Indicator 0x00 | Number of Records | records | padding | CRC32(4)
/// Each record is a pair of multibyte integers, and it is the second of each
/// pair we want.
fn xz_uncompressed_size(file: &mut File, file_len: u64) -> Result<u64> {
    const FOOTER: u64 = 12;
    if file_len < FOOTER + 12 {
        return Err(anyhow!("file is too short to be an xz stream"));
    }

    let mut footer = [0u8; FOOTER as usize];
    file.seek(SeekFrom::End(-(FOOTER as i64)))?;
    file.read_exact(&mut footer)?;
    if &footer[10..12] != b"YZ" {
        return Err(anyhow!("missing xz footer magic — not an xz stream?"));
    }

    let backward = u32::from_le_bytes([footer[4], footer[5], footer[6], footer[7]]);
    let index_len = (backward as u64 + 1) * 4;
    if index_len > file_len - FOOTER {
        return Err(anyhow!("xz index size points outside the file"));
    }

    let mut index = vec![0u8; index_len as usize];
    file.seek(SeekFrom::Start(file_len - FOOTER - index_len))?;
    file.read_exact(&mut index)?;

    let mut cur = 0usize;
    if index.first() != Some(&0x00) {
        return Err(anyhow!("xz index does not start with its indicator"));
    }
    cur += 1;

    let count = read_multibyte(&index, &mut cur)?;
    let mut total: u64 = 0;
    for _ in 0..count {
        let _unpadded = read_multibyte(&index, &mut cur)?;
        let uncompressed = read_multibyte(&index, &mut cur)?;
        total = total
            .checked_add(uncompressed)
            .ok_or_else(|| anyhow!("xz index reports an implausible total size"))?;
    }
    Ok(total)
}

/// xz multibyte integer: 7 bits per byte, little end first, high bit continues.
fn read_multibyte(buf: &[u8], cur: &mut usize) -> Result<u64> {
    let mut value: u64 = 0;
    for i in 0..9 {
        let byte = *buf
            .get(*cur)
            .ok_or_else(|| anyhow!("xz index ended mid-number"))?;
        *cur += 1;
        value |= ((byte & 0x7f) as u64) << (i * 7);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(anyhow!("xz multibyte integer is too long"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A real 105,000-byte payload compressed with `xz -9`.
    const SAMPLE: &[u8] = include_bytes!("../tests/fixtures/sample.img.xz");

    fn fixture() -> (tempdir::Dir, String) {
        let dir = tempdir::Dir::new();
        let path = format!("{}/sample.img.xz", dir.path());
        File::create(&path).unwrap().write_all(SAMPLE).unwrap();
        (dir, path)
    }

    #[test]
    fn reads_uncompressed_size_from_the_index() {
        let (_d, path) = fixture();
        // Read from a 180-byte file without decompressing 105 KB.
        assert_eq!(image_size(&path).unwrap(), 105_000);
    }

    #[test]
    fn decompresses_to_exactly_that_many_bytes() {
        let (_d, path) = fixture();
        let mut out = Vec::new();
        open_image(&path).unwrap().read_to_end(&mut out).unwrap();
        assert_eq!(out.len() as u64, image_size(&path).unwrap());
    }

    #[test]
    fn plain_files_report_their_own_length() {
        let dir = tempdir::Dir::new();
        let path = format!("{}/plain.img", dir.path());
        File::create(&path)
            .unwrap()
            .write_all(&[7u8; 4096])
            .unwrap();
        assert_eq!(image_size(&path).unwrap(), 4096);
        let mut out = Vec::new();
        open_image(&path).unwrap().read_to_end(&mut out).unwrap();
        assert_eq!(out, vec![7u8; 4096]);
    }

    #[test]
    fn truncated_or_corrupt_xz_is_rejected_not_guessed() {
        let dir = tempdir::Dir::new();
        // Footer magic intact but the index length lies about the file.
        let path = format!("{}/bad.img.xz", dir.path());
        let mut bad = SAMPLE.to_vec();
        let n = bad.len();
        bad[n - 8..n - 4].copy_from_slice(&u32::MAX.to_le_bytes());
        File::create(&path).unwrap().write_all(&bad).unwrap();
        assert!(image_size(&path).is_err());

        // Too short to contain a footer at all.
        let short = format!("{}/short.img.xz", dir.path());
        File::create(&short).unwrap().write_all(b"YZ").unwrap();
        assert!(image_size(&short).is_err());
    }

    #[test]
    fn known_archives_are_refused_rather_than_written_raw() {
        let dir = tempdir::Dir::new();
        for ext in ["gz", "bz2", "zst", "zip"] {
            let path = format!("{}/pi.img.{ext}", dir.path());
            File::create(&path)
                .unwrap()
                .write_all(b"not a raw image")
                .unwrap();
            assert!(
                image_size(&path).is_err(),
                ".{ext} must not be sized as raw"
            );
            assert!(
                open_image(&path).is_err(),
                ".{ext} must not be opened as raw"
            );
        }
    }

    #[test]
    fn tar_xz_is_refused_even_though_xz_is_supported() {
        let dir = tempdir::Dir::new();
        let path = format!("{}/backup.tar.xz", dir.path());
        File::create(&path).unwrap().write_all(SAMPLE).unwrap();
        // The xz layer would decompress fine; the payload is not an image.
        assert!(image_size(&path).is_err());
        assert!(open_image(&path).is_err());
    }

    #[test]
    fn multibyte_reader_handles_boundaries() {
        let mut cur = 0;
        assert_eq!(read_multibyte(&[0x00], &mut cur).unwrap(), 0);
        cur = 0;
        assert_eq!(read_multibyte(&[0x7f], &mut cur).unwrap(), 127);
        cur = 0;
        assert_eq!(read_multibyte(&[0x80, 0x01], &mut cur).unwrap(), 128);
        cur = 0;
        assert!(read_multibyte(&[0x80], &mut cur).is_err()); // continues past the end
        cur = 0;
        assert!(read_multibyte(&[0x80; 10], &mut cur).is_err()); // never terminates
    }

    /// Minimal scratch directory so the tests touch no shared state.
    mod tempdir {
        use std::path::PathBuf;
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new() -> Self {
                let p = std::env::temp_dir().join(format!(
                    "dd-etcher-test-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                ));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
            pub fn path(&self) -> String {
                self.0.display().to_string()
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
