// main.rs
mod api;
mod backup;
mod cli;
mod image;
mod stats;

use anyhow::Result;
use clap::Parser;
use cli::{run_cli, Args};

#[tokio::main]
async fn main() -> Result<()> {
    better_panic::install();
    
    if let Err(e) = run().await {
        eprintln!("{}", console::style("错误详情:").red().bold());
        eprintln!("{}", e);
        
        let mut source = e.source();
        while let Some(s) = source {
            eprintln!("原因: {}", s);
            source = s.source();
        }
        
        std::process::exit(1);
    }
    
    Ok(())
}

async fn run() -> Result<()> {
    let args = Args::parse();
    run_cli(args).await
}