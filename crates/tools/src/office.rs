//! Native document text extraction for DOCX, PPTX, XLSX and PDF.
//!
//! Office files are ZIP containers of XML. The tool unpacks them with the
//! system's `tar`/`unzip` and reads the text nodes, which is enough for an
//! agent to read a contract, a deck or a workbook without a full parser. PDF
//! extraction delegates to `pdftotext` when it is installed; the plugin MCP
//! servers remain the richer route for layout and rendering.

use std::path::{Path, PathBuf};

use argent_core::error::{Error, Result};
use argent_core::json::Json;
use argent_core::process::{run, CommandSpec};

use crate::tool::{arg_str, schema_object, Tool, ToolCtx, ToolResult};

/// One document-reading tool.
pub struct DocumentTool;

impl Tool for DocumentTool {
    fn name(&self) -> &str {
        "document"
    }

    fn description(&self) -> &str {
        "Read text from a DOCX, PPTX, XLSX, PDF, Markdown or plain-text file. \
         Office files are unzipped and their XML text nodes extracted; PDF uses \
         pdftotext. Use the office plugin MCP servers for layout, viewport and \
         rendering tasks."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![(
                "path",
                "string",
                "Path to a DOCX, PPTX, XLSX, PDF, Markdown or text file",
            )],
            vec!["path"],
        )
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let raw = arg_str(args, "path")?;
        let path = crate::fs_tools::resolve_arg(ctx, &raw);
        match extract_text(&path) {
            Ok(text) => Ok(ToolResult::ok(argent_core::text::truncate(&text, 40_000))),
            Err(error) => Ok(ToolResult::error(error.to_string())),
        }
    }
}

/// Extract readable text from a local document.
pub fn extract_text(path: &Path) -> Result<String> {
    if !path.is_file() {
        return Err(Error::io(format!("{} is not a file", path.display())));
    }
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "txt" | "md" | "markdown" | "json" | "csv" => argent_core::read_text_file(path),
        "docx" | "pptx" | "xlsx" => ooxml_text(path, &extension),
        "pdf" => pdf_text(path),
        other => Err(Error::config(format!(
            "unsupported document type '{}'; use docx, pptx, xlsx, pdf, md or txt",
            other
        ))),
    }
}

fn ooxml_text(path: &Path, extension: &str) -> Result<String> {
    let scratch = std::env::temp_dir().join(format!(
        "argent-office-{}-{}",
        std::process::id(),
        argent_core::hash::short_hash(path.to_string_lossy().as_bytes(), 8)
    ));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch)
        .map_err(|error| Error::io(format!("create {} failed: {}", scratch.display(), error)))?;

    let unpacked = unpack(path, &scratch);
    if let Err(error) = unpacked {
        let _ = std::fs::remove_dir_all(&scratch);
        return Err(error);
    }

    let mut out = String::new();
    let result = match extension {
        "docx" => read_parts(&scratch, &["word/document.xml"], &mut out).and_then(|_| {
            read_parts(
                &scratch,
                &["word/header1.xml", "word/footer1.xml"],
                &mut out,
            )
        }),
        "pptx" => {
            let mut slides: Vec<PathBuf> = std::fs::read_dir(scratch.join("ppt").join("slides"))
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|entry| entry.path())
                        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("xml"))
                        .collect()
                })
                .unwrap_or_default();
            slides.sort();
            for slide in slides {
                out.push_str(&format!(
                    "\n--- {} ---\n",
                    slide.file_name().unwrap_or_default().to_string_lossy()
                ));
                read_xml_text(&slide, &mut out)?;
            }
            Ok(())
        }
        "xlsx" => {
            let shared = scratch.join("xl").join("sharedStrings.xml");
            if shared.is_file() {
                out.push_str("shared strings:\n");
                read_xml_text(&shared, &mut out)?;
            }
            let sheet = scratch.join("xl").join("worksheets").join("sheet1.xml");
            if sheet.is_file() {
                out.push_str("\nsheet1:\n");
                read_xml_text(&sheet, &mut out)?;
            }
            Ok(())
        }
        _ => Ok(()),
    };
    let _ = std::fs::remove_dir_all(&scratch);
    result?;
    if out.trim().is_empty() {
        return Err(Error::config(format!(
            "{} contained no extractable text",
            path.display()
        )));
    }
    Ok(out)
}

fn unpack(path: &Path, destination: &Path) -> Result<()> {
    let path_text = path.to_string_lossy().into_owned();
    let destination_text = destination.to_string_lossy().into_owned();
    // bsdtar on Windows and unzip on Unix both understand the ZIP container
    // inside an OOXML file.
    let tar = run(&CommandSpec::new("tar")
        .arg("-xf")
        .arg(path_text.as_str())
        .arg("-C")
        .arg(destination_text.as_str())
        .timeout(std::time::Duration::from_secs(30)));
    if tar.as_ref().map(|output| output.success()).unwrap_or(false) {
        return Ok(());
    }
    let unzip = run(&CommandSpec::new("unzip")
        .arg("-q")
        .arg(path_text.as_str())
        .arg("-d")
        .arg(destination_text.as_str())
        .timeout(std::time::Duration::from_secs(30)));
    if unzip
        .as_ref()
        .map(|output| output.success())
        .unwrap_or(false)
    {
        Ok(())
    } else {
        Err(Error::io(format!(
            "cannot unzip {}; install tar or unzip",
            path.display()
        )))
    }
}

fn read_parts(root: &Path, names: &[&str], out: &mut String) -> Result<()> {
    for name in names {
        let path = root.join(name);
        if path.is_file() {
            read_xml_text(&path, out)?;
        }
    }
    Ok(())
}

fn read_xml_text(path: &Path, out: &mut String) -> Result<()> {
    let xml = argent_core::read_text_file(path)?;
    for text in xml_text_nodes(&xml) {
        if !text.trim().is_empty() {
            out.push_str(text.trim());
            out.push('\n');
        }
    }
    Ok(())
}

/// Pull text out of WordprocessingML, DrawingML and SpreadsheetML text nodes.
fn xml_text_nodes(xml: &str) -> Vec<String> {
    let mut values = Vec::new();
    for tag in ["w:t", "a:t", "t", "v"] {
        let open = format!("<{}", tag);
        let close = format!("</{}>", tag);
        let mut rest = xml;
        while let Some(start) = rest.find(&open) {
            let after_open = &rest[start..];
            let Some(gt) = after_open.find('>') else {
                break;
            };
            let content_start = start + gt + 1;
            let Some(end) = rest[content_start..].find(&close) else {
                break;
            };
            let value = &rest[content_start..content_start + end];
            if !value.contains('<') {
                values.push(decode_entities(value));
            }
            rest = &rest[content_start + end + close.len()..];
        }
    }
    values
}

fn decode_entities(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn pdf_text(path: &Path) -> Result<String> {
    let output = run(&CommandSpec::new("pdftotext")
        .arg(path.to_string_lossy().into_owned())
        .arg("-")
        .timeout(std::time::Duration::from_secs(60)))?;
    if !output.success() {
        return Err(Error::io(format!(
            "pdftotext failed: {}",
            output.stderr.trim()
        )));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_text_nodes_and_entities() {
        let xml = r#"<w:document><w:p><w:t>Hello</w:t><w:t>&amp; world</w:t></w:p><a:t>Slide</a:t></w:document>"#;
        let values = xml_text_nodes(xml);
        assert!(values.contains(&"Hello".to_string()));
        assert!(values.contains(&"& world".to_string()));
        assert!(values.contains(&"Slide".to_string()));
    }

    #[test]
    fn rejects_an_unknown_extension() {
        let path = std::env::temp_dir().join("argent-doc.bin");
        std::fs::write(&path, b"x").unwrap();
        assert!(extract_text(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
