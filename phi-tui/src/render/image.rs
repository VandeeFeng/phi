use std::io::Cursor;
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use base64::Engine;
use image::GenericImageView;

const DEFAULT_MAX_WIDTH: u16 = 60;
const CELL_WIDTH_PX: u32 = 9;
const CELL_HEIGHT_PX: u32 = 18;
const KITTY_CHUNK_SIZE: usize = 4096;
const MAX_IMAGE_DIMENSION: u32 = 16_384;
const MAX_IMAGE_ALLOCATION: u64 = 256 * 1024 * 1024;

static NEXT_IMAGE_ID: AtomicU32 = AtomicU32::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    Kitty,
    Iterm2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalImage {
    inner: Arc<TerminalImageData>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalImageData {
    id: u32,
    png_base64: String,
    mime_type: String,
    width_px: u32,
    height_px: u32,
    max_width: u16,
    max_height: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImageCellSize {
    pub(crate) columns: u16,
    pub(crate) rows: u16,
}

impl TerminalImage {
    pub fn from_base64(data: &str, mime_type: &str) -> anyhow::Result<Self> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|error| anyhow::anyhow!("invalid image Base64: {error}"))?;
        let mut reader = image::ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .map_err(|error| anyhow::anyhow!("invalid {mime_type} image: {error}"))?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
        limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
        limits.max_alloc = Some(MAX_IMAGE_ALLOCATION);
        reader.limits(limits);
        let image = reader
            .decode()
            .map_err(|error| anyhow::anyhow!("invalid {mime_type} image: {error}"))?;
        let (width_px, height_px) = image.dimensions();
        let png_base64 = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            data.to_string()
        } else {
            let mut png = Cursor::new(Vec::new());
            image
                .write_to(&mut png, image::ImageFormat::Png)
                .map_err(|error| anyhow::anyhow!("failed to convert image to PNG: {error}"))?;
            base64::engine::general_purpose::STANDARD.encode(png.into_inner())
        };
        Ok(Self {
            inner: Arc::new(TerminalImageData {
                id: NEXT_IMAGE_ID.fetch_add(1, Ordering::Relaxed).max(1),
                png_base64,
                mime_type: mime_type.to_string(),
                width_px,
                height_px,
                max_width: DEFAULT_MAX_WIDTH,
                max_height: None,
            }),
        })
    }

    pub fn with_max_size(mut self, width: u16, height: Option<u16>) -> Self {
        let image = Arc::make_mut(&mut self.inner);
        image.max_width = width.max(1);
        image.max_height = height.map(|height| height.max(1));
        self
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.inner.width_px, self.inner.height_px)
    }

    pub(crate) fn id(&self) -> u32 {
        self.inner.id
    }

    pub(crate) fn png_base64(&self) -> &str {
        &self.inner.png_base64
    }

    pub(crate) fn cell_size(&self, available_width: u16) -> ImageCellSize {
        let max_width = available_width.min(self.inner.max_width).max(1);
        let default_height = max_width.saturating_add(1) / 2;
        calculate_cell_size(
            self.inner.width_px,
            self.inner.height_px,
            max_width,
            self.inner.max_height.unwrap_or(default_height),
        )
    }

    pub(crate) fn fallback(&self) -> String {
        format!(
            "[Image: {} {}x{}]",
            self.inner.mime_type, self.inner.width_px, self.inner.height_px
        )
    }
}

pub(crate) fn detect_image_protocol() -> Option<ImageProtocol> {
    if std::env::var_os("TMUX").is_some()
        || env_lower("TERM")
            .is_some_and(|term| term.starts_with("tmux") || term.starts_with("screen"))
    {
        return None;
    }
    let term_program = env_lower("TERM_PROGRAM").unwrap_or_default();
    let term = env_lower("TERM").unwrap_or_default();
    if std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some()
        || std::env::var_os("WEZTERM_PANE").is_some()
        || std::env::var_os("WARP_SESSION_ID").is_some()
        || std::env::var_os("WARP_TERMINAL_SESSION_UUID").is_some()
        || matches!(
            term_program.as_str(),
            "kitty" | "ghostty" | "wezterm" | "warpterminal"
        )
        || term.contains("ghostty")
    {
        return Some(ImageProtocol::Kitty);
    }
    if std::env::var_os("ITERM_SESSION_ID").is_some() || term_program == "iterm.app" {
        return Some(ImageProtocol::Iterm2);
    }
    None
}

pub(crate) fn encode_kitty(image: &TerminalImage, size: ImageCellSize) -> String {
    let params = format!(
        "a=T,f=100,q=2,C=1,c={},r={},i={}",
        size.columns,
        size.rows,
        image.id()
    );
    let chunks = image
        .png_base64()
        .as_bytes()
        .chunks(KITTY_CHUNK_SIZE)
        .collect::<Vec<_>>();
    if chunks.len() == 1 {
        return format!(
            "\x1b_G{params};{}\x1b\\",
            String::from_utf8_lossy(chunks[0])
        );
    }
    chunks
        .iter()
        .enumerate()
        .map(|(index, chunk)| {
            let more = usize::from(index + 1 < chunks.len());
            let prefix = if index == 0 {
                format!("\x1b_G{params},m={more};")
            } else {
                format!("\x1b_Gm={more};")
            };
            format!("{prefix}{}\x1b\\", String::from_utf8_lossy(chunk))
        })
        .collect()
}

pub(crate) fn encode_iterm2(image: &TerminalImage, size: ImageCellSize) -> String {
    format!(
        "\x1b]1337;File=inline=1;width={};height=auto;preserveAspectRatio=1:{}\x07",
        size.columns,
        image.png_base64()
    )
}

pub(crate) fn delete_kitty_image(id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\")
}

fn calculate_cell_size(
    width_px: u32,
    height_px: u32,
    max_width: u16,
    max_height: u16,
) -> ImageCellSize {
    let width_scale = max_width as f64 * CELL_WIDTH_PX as f64 / width_px.max(1) as f64;
    let height_scale = max_height as f64 * CELL_HEIGHT_PX as f64 / height_px.max(1) as f64;
    let scale = width_scale.min(height_scale);
    ImageCellSize {
        columns: ((width_px as f64 * scale / CELL_WIDTH_PX as f64).ceil() as u16)
            .clamp(1, max_width),
        rows: ((height_px as f64 * scale / CELL_HEIGHT_PX as f64).ceil() as u16)
            .clamp(1, max_height),
    }
}

fn env_lower(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|value| value.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_image_is_normalized_with_dimensions() {
        let image = TerminalImage::from_base64(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
            "image/png",
        )
        .unwrap();

        assert_eq!(image.dimensions(), (1, 1));
        assert_eq!(
            image.cell_size(60),
            ImageCellSize {
                columns: 60,
                rows: 30
            }
        );
    }
}
