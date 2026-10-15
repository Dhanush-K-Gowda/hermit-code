//! Image attachments: clipboard capture, file loading, downscaling and `data:` URLs.
//! No image crates: macOS uses the built-in `osascript`/`sips`, Linux `wl-paste`/`xclip`.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Longest side sent to models; larger images are downscaled (macOS) to save tokens and memory.
const MAX_SIDE: u32 = 1568;
/// Most providers reject images above ~5 MB.
const MAX_BYTES: usize = 5 * 1024 * 1024;
const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

pub struct Image {
    pub data_url: String,
    /// Short description for the terminal, e.g. "png 1.2 MB → jpeg 210 KB".
    pub info: String,
}

fn mime(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

fn size(n: usize) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", n.div_ceil(1024))
    }
}

/// Load an image file, downscale it if needed, and encode it as a data URL.
pub fn load(path: &Path) -> Result<Image> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    encode(bytes, path)
}

fn encode(bytes: Vec<u8>, path: &Path) -> Result<Image> {
    let Some(kind) = mime(&bytes) else { bail!("{} is not a PNG, JPEG, GIF or WebP image", path.display()) };
    let original = bytes.len();
    let original_kind = kind;
    let (bytes, kind) = downscale(path, bytes, kind);
    if bytes.len() > MAX_BYTES {
        bail!("image is {} after resizing; the limit is {}", size(bytes.len()), size(MAX_BYTES));
    }
    let short = |m: &str| m.trim_start_matches("image/").to_string();
    let info = if bytes.len() == original {
        format!("{} {}", short(kind), size(original))
    } else {
        format!("{} {} → {} {}", short(original_kind), size(original), short(kind), size(bytes.len()))
    };
    Ok(Image { data_url: format!("data:{kind};base64,{}", base64(&bytes)), info })
}

/// On macOS, shrink images whose longest side exceeds MAX_SIDE (to JPEG). Elsewhere, or on
/// any failure, the original bytes are used.
fn downscale(path: &Path, bytes: Vec<u8>, kind: &'static str) -> (Vec<u8>, &'static str) {
    if !cfg!(target_os = "macos") || kind == "image/gif" {
        return (bytes, kind);
    }
    let dims = Command::new("sips").args(["-g", "pixelWidth", "-g", "pixelHeight"]).arg(path).output();
    let Ok(out) = dims else { return (bytes, kind) };
    let text = String::from_utf8_lossy(&out.stdout);
    let longest = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("pixelWidth:").or_else(|| l.trim().strip_prefix("pixelHeight:")))
        .filter_map(|v| v.trim().parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    if longest <= MAX_SIDE && bytes.len() <= MAX_BYTES {
        return (bytes, kind);
    }
    let out_path = std::env::temp_dir().join(format!("hermit-img-{}-{}.jpg", std::process::id(), crate::session::now()));
    let ok = Command::new("sips")
        .args(["-s", "format", "jpeg", "-s", "formatOptions", "85", "-Z", &MAX_SIDE.to_string()])
        .arg(path)
        .arg("--out")
        .arg(&out_path)
        .output()
        .is_ok_and(|o| o.status.success());
    let resized = ok.then(|| std::fs::read(&out_path).ok()).flatten();
    let _ = std::fs::remove_file(&out_path);
    match resized {
        Some(r) if !r.is_empty() => (r, "image/jpeg"),
        _ => (bytes, kind),
    }
}

/// Grab an image from the system clipboard. Ok(None) means the clipboard has no image.
pub fn from_clipboard() -> Result<Option<Image>> {
    let tmp = std::env::temp_dir().join(format!("hermit-clip-{}.png", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let got = if cfg!(target_os = "macos") {
        let script = format!(
            "try\n set d to (the clipboard as «class PNGf»)\non error\n return \"none\"\nend try\n\
             set f to open for access (POSIX file \"{}\") with write permission\n\
             set eof f to 0\nwrite d to f\nclose access f\nreturn \"ok\"",
            tmp.display()
        );
        let out = Command::new("osascript").arg("-e").arg(script).output().context("osascript not available")?;
        String::from_utf8_lossy(&out.stdout).trim() == "ok"
    } else {
        let tries: [(&str, &[&str]); 2] =
            [("wl-paste", &["--no-newline", "--type", "image/png"]), ("xclip", &["-selection", "clipboard", "-t", "image/png", "-o"])];
        tries.iter().any(|(cmd, args)| {
            Command::new(cmd)
                .args(*args)
                .output()
                .ok()
                .filter(|o| o.status.success() && mime(&o.stdout).is_some())
                .is_some_and(|o| std::fs::write(&tmp, o.stdout).is_ok())
        })
    };
    if !got {
        return Ok(None);
    }
    let img = load(&tmp);
    let _ = std::fs::remove_file(&tmp);
    img.map(Some)
}

/// Find image file paths in typed or dragged-in text (absolute, ~/, or relative to `root`;
/// optionally quoted or with backslash-escaped spaces). Returns (byte range, path) pairs.
pub fn find_paths(input: &str, root: &Path) -> Vec<(std::ops::Range<usize>, PathBuf)> {
    let mut found = Vec::new();
    let b = input.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let (token, end) = match b[i] {
            q @ (b'\'' | b'"') => match input[i + 1..].find(q as char) {
                Some(close) => (input[i + 1..i + 1 + close].to_string(), i + close + 2),
                None => break,
            },
            c if c.is_ascii_whitespace() => {
                i += 1;
                continue;
            }
            _ => {
                // Unquoted: runs until unescaped whitespace; `\ ` is an escaped space.
                let mut t = String::new();
                let mut j = i;
                while j < b.len() && !b[j].is_ascii_whitespace() {
                    if b[j] == b'\\' && j + 1 < b.len() && b[j + 1] == b' ' {
                        t.push(' ');
                        j += 2;
                    } else {
                        let ch = input[j..].chars().next().unwrap();
                        t.push(ch);
                        j += ch.len_utf8();
                    }
                }
                (t, j)
            }
        };
        i = end;
        let lower = token.to_ascii_lowercase();
        if !EXTENSIONS.iter().any(|e| lower.ends_with(&format!(".{e}"))) {
            continue;
        }
        let path = if let Some(rest) = token.strip_prefix("~/") {
            dirs::home_dir().map(|h| h.join(rest)).unwrap_or_else(|| PathBuf::from(&token))
        } else if Path::new(&token).is_absolute() {
            PathBuf::from(&token)
        } else {
            root.join(&token)
        };
        if path.is_file() {
            found.push((start..end, path));
        }
    }
    found
}

/// Standard base64 (RFC 4648) with padding.
pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Smallest valid PNG (1x1 transparent pixel).
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01,
        0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41,
        0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn loads_small_png_as_data_url() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.png");
        std::fs::write(&p, PNG).unwrap();
        let img = load(&p).unwrap();
        assert!(img.data_url.starts_with("data:image/png;base64,iVBORw0KGgo"));
        assert!(img.info.starts_with("png"));
        std::fs::write(d.path().join("fake.png"), b"not an image").unwrap();
        assert!(load(&d.path().join("fake.png")).is_err());
    }

    #[test]
    fn finds_dragged_and_typed_paths() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join("my shots")).unwrap();
        std::fs::write(root.join("my shots/one.png"), PNG).unwrap();
        std::fs::write(root.join("two.JPG"), PNG).unwrap();
        let abs = root.join("my shots/one.png");
        let escaped = abs.display().to_string().replace(' ', "\\ ");

        let input = format!("fix this {escaped} and 'two.JPG' plus missing.png and notes.txt");
        let found = find_paths(&input, root);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].1, abs);
        assert_eq!(&input[found[0].0.clone()], escaped);
        assert_eq!(found[1].1, root.join("two.JPG"));
        assert_eq!(&input[found[1].0.clone()], "'two.JPG'");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn downscales_large_images_on_macos() {
        let banner = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/banner.png");
        if !banner.exists() {
            return;
        }
        let img = load(&banner).unwrap();
        assert!(img.data_url.starts_with("data:image/jpeg;base64,"), "{}", img.info);
        assert!(img.info.starts_with("png 1.6 MB → jpeg"), "{}", img.info);
        assert!(img.data_url.len() < 1_000_000);
    }

    /// Reads (never writes) the real clipboard. Run with `cargo test clipboard -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn clipboard_probe() {
        match from_clipboard() {
            Ok(Some(img)) => println!("clipboard image: {}", img.info),
            Ok(None) => println!("clipboard has no image"),
            Err(e) => panic!("clipboard read failed: {e:#}"),
        }
    }
}
