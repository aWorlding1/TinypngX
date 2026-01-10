use crate::image::ImageResult;
use anyhow::{Context, Result};
use chrono::{Datelike, Local};
use dirs::home_dir;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::fs as tokio_fs;
use lru::LruCache;
use md5;
use tracing::{info, warn};

// API状态文件路径
fn api_state_path() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".tinypngx_state.toml")
}

// API密钥文件路径
fn api_keys_path() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Tinypng API密钥.txt")
}

// 响应缓存
static RESPONSE_CACHE: Lazy<Mutex<LruCache<String, (Vec<u8>, Instant)>>> = Lazy::new(|| {
    Mutex::new(LruCache::new(NonZeroUsize::new(100).unwrap()))
});

// API状态结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiState {
    #[serde(rename = "API状态")]
    pub key_statuses: HashMap<String, u32>, // key -> 剩余额度
    
    #[serde(rename = "元数据")]
    pub metadata: ApiMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiMetadata {
    #[serde(rename = "last_updated")]
    pub last_updated: String,
    
    #[serde(rename = "month")]
    pub current_month: String,
}

impl ApiState {
    // 创建新的API状态
    pub fn new() -> Self {
        let now = Local::now();
        Self {
            key_statuses: HashMap::new(),
            metadata: ApiMetadata {
                last_updated: now.to_rfc3339(),
                current_month: format!("{}-{:02}", now.year(), now.month()),
            },
        }
    }
    
    // 加载API状态
    pub async fn load() -> Result<Self> {
        let path = api_state_path();
        if path.exists() {
            let content = tokio_fs::read_to_string(&path)
                .await
                .context("读取API状态文件失败")?;
            
            let mut state: Self = toml::from_str(&content)
                .context("解析API状态文件失败")?;
            
            // 检查是否跨月
            let now = Local::now();
            let current_month = format!("{}-{:02}", now.year(), now.month());
            if state.metadata.current_month != current_month {
                info!("检测到月度变更，重置所有API额度");
                state.metadata.current_month = current_month;
                for (_, value) in state.key_statuses.iter_mut() {
                    *value = 500;
                }
            }
            
            Ok(state)
        } else {
            Ok(Self::new())
        }
    }
    
    // 保存API状态
    pub async fn save(&mut self) -> Result<()> {
        self.metadata.last_updated = Local::now().to_rfc3339();
        let content = toml::to_string_pretty(self)
            .context("序列化API状态失败")?;
        
        tokio_fs::write(api_state_path(), content)
            .await
            .context("写入API状态文件失败")?;
        Ok(())
    }
    
    // 更新密钥列表
    pub fn update_keys(&mut self, keys: &[String]) {
        let existing_keys: HashSet<_> = self.key_statuses.keys().cloned().collect();
        
        for key in keys {
            if !existing_keys.contains(key) {
                self.key_statuses.insert(key.clone(), 500);
            }
        }
        
        // 移除已不存在的密钥
        self.key_statuses.retain(|k, _| keys.contains(k));
    }
    
    // 获取活跃密钥数量
    pub fn active_keys_count(&self) -> usize {
        self.key_statuses.iter().filter(|(_, &v)| v > 0).count()
    }
    
    // 获取总剩余额度
    pub fn total_remaining_credits(&self) -> u32 {
        self.key_statuses.iter().map(|(_, &v)| v).sum()
    }
    
    // 更新密钥额度
    pub fn update_key_credit(&mut self, key: &str, new_credit: u32) {
        if let Some(credit) = self.key_statuses.get_mut(key) {
            *credit = new_credit;
        }
    }
    
    // 标记密钥为无效
    pub fn mark_key_invalid(&mut self, key: &str) {
        self.key_statuses.insert(key.to_string(), 0);
        warn!("密钥 {} 已标记为无效", key);
    }
}

#[derive(Debug, Deserialize)]
struct TinifyResponse {
    input: TinifyInput,
    output: TinifyOutput,
}

#[derive(Debug, Deserialize)]
struct TinifyInput {
    size: u64,
    r#type: String,
}

#[derive(Debug, Deserialize)]
struct TinifyOutput {
    size: u64,
    r#type: String,
    width: u32,
    height: u32,
    ratio: f64,
    url: String,
}

#[derive(Clone)]
pub struct TinifyClient {
    client: reqwest::Client,
    keys: Vec<String>,
    state: Arc<Mutex<ApiState>>,
    key_stats: Arc<Mutex<HashMap<String, (u32, Instant)>>>,
}

impl TinifyClient {
    pub async fn new(keys: Vec<String>, initial_state: ApiState) -> Result<Self> {
        let mut key_stats = HashMap::new();
        for key in &keys {
            key_stats.insert(key.clone(), (0, Instant::now()));
        }
        
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .pool_idle_timeout(Duration::from_secs(300))
            .tcp_keepalive(Duration::from_secs(60))
            .tcp_keepalive_interval(Duration::from_secs(30))
            .build()
            .context("创建HTTP客户端失败")?;
        
        Ok(Self {
            client,
            keys,
            state: Arc::new(Mutex::new(initial_state)),
            key_stats: Arc::new(Mutex::new(key_stats)),
        })
    }

    // 使用预加载的数据压缩图片
    pub async fn compress_with_data(&self, path: &Path, data: &[u8], original_size: u64) -> Result<ImageResult> {
        // 检查缓存
        let cache_key = format!("{:x}", md5::compute(data));
        let cached_data = {
            let mut cache = RESPONSE_CACHE.lock().unwrap();
            cache.get(&cache_key)
                .filter(|(_, timestamp)| timestamp.elapsed() < Duration::from_secs(3600))
                .map(|(data, _)| data.clone())
        };
        
        if let Some(compressed_data) = cached_data {
            tokio_fs::write(path, &compressed_data)
                .await
                .context(format!("写入文件失败: {:?}", path))?;
                
            return Ok(ImageResult::new(
                path.to_path_buf(), 
                true, 
                "压缩成功 (来自缓存)",
                original_size,
                compressed_data.len() as u64
            ));
        }
        
        // 选择最佳密钥
        let key = self.select_best_key().await;
        let active_key_count = self.get_active_key_count().await;
        
        // 调用TinyPNG API
        let url = "https://api.tinify.com/shrink";
        let response = self
            .client
            .post(url)
            .basic_auth("api", Some(&key))
            .body(data.to_vec())
            .send()
            .await;

        match response {
            Ok(resp) => {
                let status = resp.status();
                
                // 处理API错误
                if status.as_u16() == 429 {
                    self.mark_key_invalid(&key).await;
                    return Ok(ImageResult::new(
                        path.to_path_buf(),
                        false,
                        format!("额度耗尽，已移除该 key，剩余可用 key: {}", active_key_count - 1),
                        original_size,
                        0
                    ));
                } else if status.as_u16() == 401 {
                    self.mark_key_invalid(&key).await;
                    return Ok(ImageResult::new(
                        path.to_path_buf(),
                        false,
                        format!("密钥无效，已移除该 key，剩余可用 key: {}", active_key_count - 1),
                        original_size,
                        0
                    ));
                }
                
                // 处理成功响应
                if status.is_success() {
                    // 从响应头获取压缩计数
                    let compression_count = resp.headers()
                        .get("Compression-Count")
                        .and_then(|h| h.to_str().ok())
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(0);
                    
                    // 计算剩余额度
                    let remaining_credits = 500u32.saturating_sub(compression_count);
                    
                    // 更新API状态
                    self.update_key_credit(&key, remaining_credits).await;
                    
                    let tinify_response: TinifyResponse = resp
                        .json()
                        .await
                        .context("解析Tinify响应失败")?;
                    
                    // 下载压缩后的图片
                    let compressed_data = self
                        .client
                        .get(&tinify_response.output.url)
                        .send()
                        .await
                        .context("下载压缩图片失败")?
                        .bytes()
                        .await
                        .context("获取压缩图片数据失败")?;
                    
                    // 缓存响应
                    {
                        let mut cache = RESPONSE_CACHE.lock().unwrap();
                        cache.put(cache_key, (compressed_data.clone().to_vec(), Instant::now()));
                    }
                    
                    // 保存压缩后的图片
                    tokio_fs::write(path, compressed_data.clone())
                        .await
                        .context(format!("写入文件失败: {:?}", path))?;
                    
                    // 更新密钥使用统计
                    self.update_key_stats(&key, true).await;
                    
                    return Ok(ImageResult::new(
                        path.to_path_buf(), 
                        true, 
                        format!("压缩成功: {} → {} (节省 {:.1}%)，剩余额度: {}", 
                            original_size, 
                            tinify_response.output.size,
                            (1.0 - tinify_response.output.ratio) * 100.0,
                            remaining_credits),
                        original_size,
                        tinify_response.output.size
                    ));
                }

                // 其它HTTP错误
                let msg = resp.text().await.unwrap_or_default();
                self.update_key_stats(&key, false).await;
                Ok(ImageResult::new(
                    path.to_path_buf(),
                    false,
                    format!("HTTP {}: {}", status, msg),
                    original_size,
                    0
                ))
            }
            Err(e) => {
                self.update_key_stats(&key, false).await;
                Ok(ImageResult::new(
                    path.to_path_buf(),
                    false,
                    format!("网络错误: {}", e),
                    original_size,
                    0
                ))
            }
        }
    }

    // 直接压缩图片
    pub async fn compress(&self, path: &Path) -> Result<ImageResult> {
        if !path.exists() {
            return Ok(ImageResult::new(
                path.to_path_buf(),
                false,
                "文件不存在",
                0,
                0
            ));
        }

        let bytes = tokio_fs::read(path)
            .await
            .context(format!("读取文件失败: {:?}", path))?;
        let original_size = bytes.len() as u64;
        
        self.compress_with_data(path, &bytes, original_size).await
    }
    
    // 获取活跃密钥数量
    async fn get_active_key_count(&self) -> usize {
        let state = self.state.lock().unwrap();
        state.active_keys_count()
    }
    
    // 智能密钥选择算法
    async fn select_best_key(&self) -> String {
        // 获取状态快照
        let (key_statuses, key_stats) = {
            let state = self.state.lock().unwrap();
            let key_statuses = state.key_statuses.clone();
            let key_stats = self.key_stats.lock().unwrap().clone();
            (key_statuses, key_stats)
        };
        
        // 过滤出还有额度的密钥
        let available_keys: Vec<String> = self.keys.iter()
            .filter(|k| key_statuses.get(*k).copied().unwrap_or(0) > 0)
            .cloned()
            .collect();
        
        if available_keys.is_empty() {
            warn!("没有可用API密钥，使用第一个密钥尝试");
            return self.keys.first().cloned().unwrap_or_default();
        }
        
        // 优先选择最近使用较少且成功率高的密钥
        let mut best_key = available_keys[0].clone();
        let mut best_score = f64::MIN;
        
        for key in &available_keys {
            let remaining_credits = key_statuses.get(key).copied().unwrap_or(0) as f64;
            let (success_count, last_used) = key_stats.get(key).cloned().unwrap_or((0, Instant::now()));
            
            let success_rate = if success_count > 0 {
                success_count as f64 / (success_count + 1) as f64
            } else {
                0.5
            };
            
            // 最近使用加分
            let time_score = -(last_used.elapsed().as_secs() as i64) as f64 / 3600.0;
            
            // 剩余额度加分
            let credit_score = remaining_credits / 500.0;
            
            // 综合评分
            let score = success_rate * 0.4 + time_score * 0.3 + credit_score * 0.3;
            
            if score > best_score {
                best_score = score;
                best_key = key.clone();
            }
        }
        
        best_key
    }
    
    // 更新密钥使用统计
    async fn update_key_stats(&self, key: &str, success: bool) {
        let mut stats = self.key_stats.lock().unwrap();
        let entry = stats.entry(key.to_string())
            .or_insert((0, Instant::now()));
        
        if success {
            entry.0 += 1;
        }
        entry.1 = Instant::now();
    }
    
    // 标记密钥无效（原子操作）
    async fn mark_key_invalid(&self, key: &str) {
        let mut state = self.state.lock().unwrap();
        state.mark_key_invalid(key);
    }
    
    // 更新密钥额度（原子操作）
    async fn update_key_credit(&self, key: &str, credit: u32) {
        let mut state = self.state.lock().unwrap();
        state.update_key_credit(key, credit);
    }
}

/// 读取密钥文件
pub fn keys() -> Result<Vec<String>> {
    let path = api_keys_path();
    if !path.exists() {
        fs::File::create(&path)
            .context("创建API密钥文件失败")?;
        return Ok(Vec::new());
    }
    
    let content = fs::read_to_string(&path)
        .context("读取API密钥文件失败")?;
    
    Ok(content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect())
}
