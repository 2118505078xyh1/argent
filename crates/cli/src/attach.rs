//! Image attachments for a user turn.
//!
//! Two ways to attach an image: write `@path` anywhere in the request, or run
//! `/image path` to queue it for the next turn. Both end up as `Content::Image`
//! parts on the user message.
//!
//! The file is inlined as a `data:` URL rather than referenced by path, because
//! a provider has no access to this machine's filesystem.

use std::path::{Path, PathBuf};

use argent_ai::Content;

/// An image the operator attached, kept with its path for display.
#[derive(Debug, Clone)]
pub struct Attachment {
    /// Path as the operator wrote it.
    pub label: String,
    /// The message part.
    pub part: Content,
}

/// Larger files are almost never worth the context they consume.
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

/// Read an image from disk into a message part.
pub fn load(path: &Path) -> Result<Attachment, String> {
    let mime = argent_core::base64::image_mime_for_path(path).ok_or_else(|| {
        format!(
            "{}: not an image (png, jpg, gif, webp, bmp)",
            path.display()
        )
    })?;
    let metadata = std::fs::metadata(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    if !metadata.is_file() {
        return Err(format!("{}: not a file", path.display()));
    }
    if metadata.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "{}: {} MiB is over the {} MiB limit",
            path.display(),
            metadata.len() / (1024 * 1024),
            MAX_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    let encoded = argent_core::base64::encode(&bytes);
    Ok(Attachment {
        label: path.display().to_string(),
        part: Content::Image {
            url: format!("data:{};base64,{}", mime, encoded),
            detail: None,
        },
    })
}

/// Find `@path` references in the request text and load them.
///
/// A token counts as an attachment only when the file exists and has an image
/// extension. Anything else stays in the text untouched, so `@src/main.rs` is
/// still just a mention and the model reads it with its own tools.
pub fn collect(text: &str, cwd: &Path) -> (Vec<Attachment>, Vec<String>) {
    let mut attachments = Vec::new();
    let mut problems = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for token in text.split_whitespace() {
        let Some(raw) = token.strip_prefix('@') else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        // Trailing punctuation from prose ("@shot.png," / "@shot.png。") is not
        // part of the filename.
        let trimmed = raw.trim_end_matches(|c: char| {
            matches!(
                c,
                ',' | '.' | ';' | ':' | ')' | ']' | '"' | '\'' | '。' | '，' | '）'
            )
        });
        if trimmed.is_empty() {
            continue;
        }
        let path = argent_core::path::resolve(cwd, Path::new(trimmed));
        if argent_core::base64::image_mime_for_path(&path).is_none() || !path.is_file() {
            continue;
        }
        if seen.iter().any(|existing| existing == &path) {
            continue;
        }
        seen.push(path.clone());
        match load(&path) {
            Ok(attachment) => attachments.push(attachment),
            Err(message) => problems.push(message),
        }
    }
    (attachments, problems)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_png(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("argent-attach-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        // A one-pixel PNG is not needed: the loader never decodes the image.
        file.write_all(b"\x89PNG\r\n\x1a\nfake").unwrap();
        path
    }

    #[test]
    fn a_mentioned_image_is_collected() {
        let path = temp_png("collect.png");
        let text = format!("what is wrong here? @{}", path.display());
        let (attachments, problems) = collect(&text, Path::new("."));
        assert!(problems.is_empty());
        assert_eq!(attachments.len(), 1);
        assert!(matches!(attachments[0].part, Content::Image { .. }));
    }

    #[test]
    fn a_mentioned_source_file_is_left_alone() {
        let dir = std::env::temp_dir().join("argent-attach-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("code.rs");
        std::fs::write(&path, "fn main() {}").unwrap();
        let text = format!("review @{}", path.display());
        let (attachments, _) = collect(&text, Path::new("."));
        assert!(attachments.is_empty());
    }

    #[test]
    fn the_same_image_twice_is_attached_once() {
        let path = temp_png("twice.png");
        let text = format!("@{} and again @{}", path.display(), path.display());
        let (attachments, _) = collect(&text, Path::new("."));
        assert_eq!(attachments.len(), 1);
    }

    #[test]
    fn trailing_punctuation_is_not_part_of_the_name() {
        let path = temp_png("punct.png");
        let text = format!("look at @{}, then answer.", path.display());
        let (attachments, _) = collect(&text, Path::new("."));
        assert_eq!(attachments.len(), 1, "path with punctuation should load");
    }

    #[test]
    fn a_missing_file_is_not_an_attachment() {
        let (attachments, _) = collect("@nope.png", Path::new("/definitely/not/here"));
        assert!(attachments.is_empty());
    }

    #[test]
    fn loading_a_non_image_is_an_error() {
        let dir = std::env::temp_dir().join("argent-attach-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notes.txt");
        std::fs::write(&path, "hi").unwrap();
        assert!(load(&path).is_err());
    }
}
