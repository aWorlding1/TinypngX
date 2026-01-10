// cli.rs
use crate::{
    api::{keys, ApiState, TinifyClient},
    backup,
    image::{find_images, ImageResult},
    stats::print_summary,
};
use anyhow::{bail, Context, Result};
use clap::Parser;
use comfy_table::{Cell, Table};
use console::style;
use dialoguer::{Confirm, Input};
use futures::future::join_all;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::{Path, PathBuf};
use tokio::sync::Semaphore;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(name = "tinypngx-rs")]
#[command(about = "高并发 TinyPNG 图片压缩工具")]
pub struct Args {
    /// 要处理的目录
    #[arg(default_value = ".")]
    pub directory: PathBuf,

    /// 递归搜索子目录
    #[arg(short, long)]
    pub recursive: bool,

    /// 是否创建备份
    #[arg(short, long, default_value_t = true)]
    pub backup: bool,

    /// 关闭校准模式（使用历史状态直接运行）
    #[arg(long)]
    pub no_calibration: bool,
}

pub async fn run_cli(args: Args) -> Result<()> {
    // 确保目录存在
    let dir = ensure_directory(args.directory.clone())
        .context("无法确定工作目录")?;
    
    // 检查目录权限
    check_directory_permissions(&dir)
        .context("目录权限检查失败")?;

    // 读取 API key
    let keys = keys()
        .context("无法读取API密钥文件")?;
    if keys.is_empty() {
        bail!("Tinypng API密钥.txt 中没有找到任何 key");
    }

    // 查找图片
    let images = find_images(&dir, args.recursive)
        .await
        .context("查找图片文件失败")?;
    if images.is_empty() {
        bail!("未找到支持的图片文件");
    }
    println!("找到 {} 张图片", style(images.len()).green().bold());

    // 预加载图片到内存（默认开启）
    let preloaded_images = preload_images(&images).await?;
    if preloaded_images.len() < images.len() {
        eprintln!("{} 张图片预加载失败，将尝试直接处理", images.len() - preloaded_images.len());
    }

    // 加载API状态
    let mut api_state = ApiState::load().await?;
    api_state.update_keys(&keys);
    
    // 打印配置
    print_config_table(images.len(), &args, &api_state);

    // 二次确认
    if !Confirm::new()
        .with_prompt("确认开始处理？")
        .default(true)
        .interact()?
    {
        println!("{}", style("操作已取消").yellow());
        return Ok(());
    }

    // 初始化客户端并压缩
    let client = TinifyClient::new(keys, api_state.clone())
        .await
        .context("创建Tinify客户端失败")?;
    
    // 根据API额度分配任务并压缩
    let results = compress_all(
        images, 
        preloaded_images, 
        client, 
        args.backup, 
        args.no_calibration,
        &mut api_state
    ).await.context("图片压缩处理失败")?;

    // 保存API状态
    api_state.save().await?;

    // 汇总结果
    print_summary(results).await;
    Ok(())
}

fn ensure_directory(dir: PathBuf) -> Result<PathBuf> {
    let current_dir = std::env::current_dir()
        .context("无法获取当前工作目录")?;
    
    let target_dir = if dir.is_absolute() {
        dir
    } else {
        current_dir.join(dir)
    };
    
    if !target_dir.exists() {
        println!("{}", style("警告: 目录不存在").yellow());
        let input: String = Input::new()
            .with_prompt("请输入有效目录路径")
            .interact_text()
            .context("获取用户输入失败")?;
        
        let new_dir = PathBuf::from(input.trim());
        if new_dir.is_absolute() {
            if !new_dir.exists() {
                bail!("目录不存在: {:?}", new_dir);
            }
            Ok(new_dir)
        } else {
            let abs_new_dir = current_dir.join(new_dir);
            if !abs_new_dir.exists() {
                bail!("目录不存在: {:?}", abs_new_dir);
            }
            Ok(abs_new_dir)
        }
    } else {
        Ok(target_dir.canonicalize().context("无法规范化路径")?)
    }
}

fn check_directory_permissions(dir: &Path) -> Result<()> {
    std::fs::read_dir(dir)
        .context("无法读取目录，请检查权限")?;
    
    let test_file = dir.join(".permission_test");
    match std::fs::File::create(&test_file) {
        Ok(_) => {
            std::fs::remove_file(&test_file)
                .context("无法删除测试文件")?;
            Ok(())
        }
        Err(e) => bail!("没有写权限: {}", e),
    }
}

fn print_config_table(count: usize, args: &Args, api_state: &ApiState) {
    let mut table = Table::new();
    table.load_preset(comfy_table::presets::ASCII_BORDERS_ONLY_CONDENSED);
    table.set_header(vec![Cell::new("选项"), Cell::new("值")]);
    table.add_row(vec![Cell::new("图片数量"), Cell::new(count.to_string())]);
    table.add_row(vec![
        Cell::new("递归搜索"),
        Cell::new(if args.recursive { "是" } else { "否" }),
    ]);
    table.add_row(vec![
        Cell::new("创建备份"),
        Cell::new(if args.backup { "是" } else { "否" }),
    ]);
    table.add_row(vec![
        Cell::new("校准模式"),
        Cell::new(if args.no_calibration { "关闭" } else { "开启" }),
    ]);
    table.add_row(vec![
        Cell::new("可用API密钥"),
        Cell::new(api_state.active_keys_count().to_string()),
    ]);
    table.add_row(vec![
        Cell::new("预估总额度"),
        Cell::new(api_state.total_remaining_credits().to_string()),
    ]);
    println!("{table}");
}

async fn preload_images(paths: &[PathBuf]) -> Result<Vec<(PathBuf, Vec<u8>)>> {
    let pb = ProgressBar::new(paths.len() as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} 预加载图片 [{bar:40.cyan/blue}] {pos}/{len}",
        )
        .unwrap()
        .progress_chars("##-"),
    );
    
    let mut results = Vec::with_capacity(paths.len());
    
    for path in paths {
        match tokio::fs::read(path).await {
            Ok(data) => {
                results.push((path.clone(), data));
                pb.inc(1);
            }
            Err(e) => {
                eprintln!("预加载失败 {}: {}", path.display(), e);
            }
        }
    }
    
    pb.finish_and_clear();
    Ok(results)
}

async fn compress_all(
    images: Vec<PathBuf>,
    preloaded: Vec<(PathBuf, Vec<u8>)>,
    client: TinifyClient,
    backup: bool,
    no_calibration: bool,
    api_state: &mut ApiState,
) -> Result<Vec<ImageResult>> {
    // 创建路径到数据的映射
    let data_map: std::collections::HashMap<_, _> = preloaded.into_iter().collect();
    
    // 根据API额度和图片数量确定并发数
    let total_images = images.len();
    let concurrency = std::cmp::min(total_images, 200); // 设置最大并发限制，避免资源耗尽
    let sem = Arc::new(Semaphore::new(concurrency));
    
    let pb = ProgressBar::new(total_images as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})",
        )
        .unwrap()
        .progress_chars("##-"),
    );

    // 校准模式：初始只处理与API数量相等的任务来校准额度
    let (initial_tasks, remaining_tasks) = if !no_calibration && total_images > api_state.active_keys_count() {
        let split_idx = api_state.active_keys_count();
        images.split_at(split_idx)
    } else {
        (&images[..0], &images[..])
    };

    // 处理初始校准任务
    let mut results = Vec::new();
    if !initial_tasks.is_empty() {
        println!("{}", style("执行校准任务以确定API额度...").yellow());
        let initial_results = process_tasks(
            initial_tasks.to_vec(), 
            &data_map, 
            &client, 
            backup, 
            &sem, 
            pb.clone()
        ).await?;
        
        results.extend(initial_results);
        
        // 校准后更新API状态
        api_state.save().await?;
        println!("{}", style("校准完成，继续处理剩余任务...").green());
    }

    // 处理剩余任务
    let remaining_results = process_tasks(
        remaining_tasks.to_vec(), 
        &data_map, 
        &client, 
        backup, 
        &sem, 
        pb.clone()
    ).await?;
    
    results.extend(remaining_results);
    
    pb.finish_and_clear();
    Ok(results)
}

async fn process_tasks(
    images: Vec<PathBuf>,
    data_map: &std::collections::HashMap<PathBuf, Vec<u8>>,
    client: &TinifyClient,
    backup: bool,
    sem: &Arc<Semaphore>,
    pb: ProgressBar,
) -> Result<Vec<ImageResult>> {
    let tasks = images.into_iter().map(|path| {
        let client = client.clone();
        let pb = pb.clone();
        let sem = sem.clone();
        let data = data_map.get(&path).cloned();
        let backup = backup;
        let path_clone = path.clone(); // 克隆路径
        
        tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            
            let res = if let Some(file_data) = data {
                if backup {
                    let original_size = file_data.len() as u64;
                    let path_for_closure = path_clone.clone(); // 为闭包再克隆一次
                    backup::with_memory_backup(&path_clone, move |_| {
                        let client = client.clone();
                        async move { 
                            client.compress_with_data(&path_for_closure, &file_data, original_size).await
                        }
                    })
                    .await
                } else {
                    let original_size = file_data.len() as u64;
                    client.compress_with_data(&path_clone, &file_data, original_size).await
                }
            } else {
                // 处理预加载失败的文件
                client.compress(&path_clone).await
            };
            
            pb.inc(1);
            res
        })
    });

    let outputs = join_all(tasks).await;
    
    let mut results = Vec::with_capacity(outputs.len());
    for out in outputs {
        match out {
            Ok(Ok(result)) => results.push(result),
            Ok(Err(e)) => {
                eprintln!("处理错误: {}", e);
            }
            Err(e) => {
                eprintln!("任务执行错误: {}", e);
            }
        }
    }
    Ok(results)
}
