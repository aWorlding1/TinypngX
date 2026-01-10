// backup.rs
use crate::image::ImageResult;
use anyhow::Result;
use std::path::Path;
use tokio::fs;

// 内存备份函数 - 默认使用
pub async fn with_memory_backup<F, Fut>(path: &Path, f: F) -> Result<ImageResult>
where
    F: FnOnce(&[u8]) -> Fut,
    Fut: std::future::Future<Output = Result<ImageResult>> + Send,
{
    // 读取文件到内存
    let original_data = fs::read(path).await?;
    
    // 执行压缩函数，传入内存数据
    let res = f(&original_data).await;

    // 若失败，从内存还原文件
    if let Ok(r) = &res {
        if !r.ok {
            fs::write(path, &original_data).await?;
        }
    }
    res
}