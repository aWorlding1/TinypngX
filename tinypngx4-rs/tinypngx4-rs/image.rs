// image.rs
use anyhow::Result;
use std::path::{Path, PathBuf};
use tokio::fs;

pub const SUPPORTED: &[&str] = &["png", "jpg", "jpeg", "webp", "avif"];

fn is_supported(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| SUPPORTED.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

// 查找图片
pub async fn find_images(root: &Path, recursive: bool) -> Result<Vec<PathBuf>> {
    let mut images = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(current) = stack.pop() {
        let mut rd = fs::read_dir(&current).await?;
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            if entry.file_type().await?.is_dir() {
                if recursive {
                    stack.push(path);
                }
            } else if is_supported(&path) {
                images.push(path);
            }
        }
    }
    
    Ok(images)
}

pub struct ImageResult {
    pub path: PathBuf,
    pub ok: bool,
    pub msg: String,
    pub original_size: u64,
    pub compressed_size: u64,
}

impl ImageResult {
    pub fn new(
        path: PathBuf, 
        ok: bool, 
        msg: impl Into<String>,
        original_size: u64,
        compressed_size: u64
    ) -> Self {
        Self {
            path,
            ok,
            msg: msg.into(),
            original_size,
            compressed_size,
        }
    }
}