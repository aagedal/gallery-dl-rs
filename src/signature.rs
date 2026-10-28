//! Upstream `SIGNATURE_CHECKS` (downloader `adjust-extensions`): detect a
//! file's real type from its first bytes and correct wrong extensions.

fn html(s: &[u8]) -> bool {
    let s = &s[..s.len().min(14)];
    let start = s
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(s.len());
    let s = s[start..].to_ascii_lowercase();
    !s.is_empty() && b"<!doctype html".starts_with(&s)
}

fn at(s: &[u8], start: usize, end: usize) -> &[u8] {
    s.get(start..end.min(s.len())).unwrap_or_default()
}

type Check = fn(&[u8]) -> bool;

/// Extension checks in upstream order.
const CHECKS: &[(&str, Check)] = &[
    ("jpg", |s| at(s, 0, 3) == b"\xFF\xD8\xFF"),
    ("png", |s| at(s, 0, 8) == b"\x89PNG\r\n\x1A\n"),
    ("gif", |s| matches!(at(s, 0, 6), b"GIF87a" | b"GIF89a")),
    ("bmp", |s| at(s, 0, 2) == b"BM"),
    ("webp", |s| {
        at(s, 0, 4) == b"RIFF" && at(s, 8, 12) == b"WEBP"
    }),
    ("avif", |s| {
        at(s, 4, 11) == b"ftypavi" && s.get(11).is_some_and(|c| b"fs".contains(c))
    }),
    ("heic", |s| {
        at(s, 4, 10) == b"ftyphe"
            && matches!(
                at(s, 10, 12),
                b"ic" | b"im" | b"is" | b"ix" | b"vc" | b"vm" | b"vs"
            )
    }),
    ("svg", |s| at(s, 0, 5) == b"<?xml"),
    ("ico", |s| at(s, 0, 4) == b"\x00\x00\x01\x00"),
    ("cur", |s| at(s, 0, 4) == b"\x00\x00\x02\x00"),
    ("psd", |s| at(s, 0, 4) == b"8BPS"),
    ("mp4", |s| {
        at(s, 4, 8) == b"ftyp" && matches!(at(s, 8, 11), b"mp4" | b"avc" | b"iso")
    }),
    ("m4v", |s| at(s, 4, 11) == b"ftypM4V"),
    ("mov", |s| at(s, 4, 12) == b"ftypqt  "),
    ("webm", |s| at(s, 0, 4) == b"\x1A\x45\xDF\xA3"),
    ("ogg", |s| at(s, 0, 4) == b"OggS"),
    ("wav", |s| at(s, 0, 4) == b"RIFF" && at(s, 8, 12) == b"WAVE"),
    ("mp3", |s| {
        at(s, 0, 3) == b"ID3" || matches!(at(s, 0, 2), b"\xFF\xFB" | b"\xFF\xF3" | b"\xFF\xF2")
    }),
    ("aac", |s| matches!(at(s, 0, 2), b"\xFF\xF9" | b"\xFF\xF1")),
    ("m3u8", |s| at(s, 0, 7) == b"#EXTM3U"),
    ("mpd", |s| s.windows(4).any(|w| w == b"<MPD")),
    ("zip", |s| {
        matches!(at(s, 0, 4), b"PK\x03\x04" | b"PK\x05\x06" | b"PK\x07\x08")
    }),
    ("rar", |s| at(s, 0, 6) == b"Rar!\x1A\x07"),
    ("7z", |s| at(s, 0, 6) == b"\x37\x7A\xBC\xAF\x27\x1C"),
    ("pdf", |s| at(s, 0, 5) == b"%PDF-"),
    ("swf", |s| matches!(at(s, 0, 3), b"CWS" | b"FWS")),
    ("html", html),
    ("htm", html),
    ("blend", |s| at(s, 0, 7) == b"BLENDER"),
    ("obj", |s| at(s, 0, 11) == b"# Blender v"),
    ("clip", |s| at(s, 0, 8) == b"CSFCHUNK"),
    ("bin", |_| false),
];

/// The extension a file with `extension` and these first 16 bytes should
/// have instead, if its extension is checked and does not match.
pub fn adjusted_extension(extension: &str, header: &[u8]) -> Option<&'static str> {
    let (_, check) = CHECKS.iter().find(|(ext, _)| *ext == extension)?;
    if check(header) {
        return None;
    }
    CHECKS
        .iter()
        .find(|(_, check)| check(header))
        .map(|(ext, _)| *ext)
}

/// Rename a downloaded file whose content does not match its extension.
/// Returns the new path and whether a file already existed there (the
/// download is then removed, like upstream's skip after adjustment).
pub fn adjust_file(path: &std::path::Path) -> std::io::Result<Option<(std::path::PathBuf, bool)>> {
    use std::io::Read;
    let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
        return Ok(None);
    };
    let mut header = [0u8; 16];
    let mut file = std::fs::File::open(path)?;
    let mut filled = 0;
    while filled < header.len() {
        match file.read(&mut header[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    let Some(adjusted) = adjusted_extension(&extension.to_ascii_lowercase(), &header[..filled])
    else {
        return Ok(None);
    };
    let target = path.with_extension(adjusted);
    if std::fs::symlink_metadata(&target).is_ok() {
        std::fs::remove_file(path)?;
        return Ok(Some((target, true)));
    }
    std::fs::rename(path, &target)?;
    Ok(Some((target, false)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_mismatched_extensions() {
        assert_eq!(
            adjusted_extension("jpg", b"\x89PNG\r\n\x1A\n...."),
            Some("png")
        );
        assert_eq!(adjusted_extension("png", b"\x89PNG\r\n\x1A\n...."), None);
        assert_eq!(adjusted_extension("bin", b"GIF89a"), Some("gif"));
        assert_eq!(
            adjusted_extension("jpg", b"  <!DOCTYPE html>"),
            Some("html")
        );
        assert_eq!(adjusted_extension("jpg", b"unknown"), None);
        assert_eq!(adjusted_extension("txt", b"GIF89a"), None);
        assert_eq!(
            adjusted_extension("mp4", b"\x1A\x45\xDF\xA3abcdefgh"),
            Some("webm")
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jpg");
        std::fs::write(&path, b"GIF89a....").unwrap();
        let (target, existed) = adjust_file(&path).unwrap().unwrap();
        assert_eq!(target, dir.path().join("a.gif"));
        assert!(!existed && target.is_file() && !path.exists());
    }
}
