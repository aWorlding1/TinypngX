// stats.rs
use crate::image::ImageResult;
use comfy_table::{Cell, Table};
use console::style;

pub async fn print_summary(results: Vec<ImageResult>) {
    let success = results.iter().filter(|r| r.ok).count();
    let failed = results.len() - success;
    
    let total_original: u64 = results.iter().map(|r| r.original_size).sum();
    let total_compressed: u64 = results.iter().map(|r| r.compressed_size).sum();
    let total_saved = total_original - total_compressed;
    let total_saved_percent = if total_original > 0 {
        (total_saved as f64 / total_original as f64) * 100.0
    } else {
        0.0
    };

    let mut summary_table = Table::new();
    summary_table.load_preset(comfy_table::presets::ASCII_BORDERS_ONLY_CONDENSED);
    summary_table.set_header(vec![Cell::new("状态"), Cell::new("数量"), Cell::new("百分比")]);
    summary_table.add_row(vec![
        Cell::new("成功").fg(comfy_table::Color::Green),
        Cell::new(success),
        Cell::new(format!("{:.1}%", success as f64 / results.len() as f64 * 100.0)),
    ]);
    summary_table.add_row(vec![
        Cell::new("失败").fg(comfy_table::Color::Red),
        Cell::new(failed),
        Cell::new(format!("{:.1}%", failed as f64 / results.len() as f64 * 100.0)),
    ]);
    summary_table.add_row(vec![Cell::new("总计"), Cell::new(results.len()), Cell::new("100%")]);
    println!("{summary_table}");

    let mut size_table = Table::new();
    size_table.load_preset(comfy_table::presets::ASCII_BORDERS_ONLY_CONDENSED);
    size_table.set_header(vec![
        Cell::new("类型"),
        Cell::new("大小 (KB)"),
        Cell::new("节省 (KB)"),
        Cell::new("节省比例")
    ]);
    size_table.add_row(vec![
        Cell::new("原始总大小"),
        Cell::new(format!("{:.2}", total_original as f64 / 1024.0)),
        Cell::new("-"),
        Cell::new("-"),
    ]);
    size_table.add_row(vec![
        Cell::new("压缩后总大小"),
        Cell::new(format!("{:.2}", total_compressed as f64 / 1024.0)),
        Cell::new(format!("{:.2}", total_saved as f64 / 1024.0)),
        Cell::new(format!("{:.1}%", total_saved_percent)),
    ]);
    println!("{size_table}");

    if failed > 0 {
        let mut err_table = Table::new();
        err_table.load_preset(comfy_table::presets::ASCII_BORDERS_ONLY_CONDENSED);
        err_table.set_header(vec![Cell::new("文件"), Cell::new("错误")]);
        for r in results.iter().filter(|r| !r.ok) {
            err_table.add_row(vec![
                Cell::new(r.path.file_name().unwrap().to_string_lossy()),
                Cell::new(&r.msg),
            ]);
        }
        println!("{}", style("错误详情").red().bold());
        println!("{err_table}");
    }

    println!("{}", style("处理完成，按 Enter 退出...").yellow().bold());
    let _ = std::io::stdin().read_line(&mut String::new());
}