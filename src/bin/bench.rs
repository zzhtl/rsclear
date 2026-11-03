//! Creates only explicitly requested, project-local benchmark fixtures.
use anyhow::{Context, Result, bail};
use rsclear::{
    model::ScanEvent,
    scan::{self, ScanRequest},
};
use std::{fs, path::PathBuf};
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        bail!("用法: rsclear-bench generate <文件数> [flat] | scan <线程数> [flat]");
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".build/benchmark");
    fs::create_dir_all(&root)?;
    let flat = args.get(2).is_some_and(|s| s == "flat");
    let fixture = root.join(if flat { "flat" } else { "tree" });
    if args[0] == "generate" {
        let count: usize = args[1].parse()?;
        if count > 2_000_000 {
            bail!("单个测试集最多 200 万文件");
        }
        fs::create_dir_all(&fixture)?;
        for n in 0..count {
            let parent = if flat {
                fixture.clone()
            } else {
                fixture.join(format!("{:04}", n / 1000))
            };
            if n % 1000 == 0 {
                fs::create_dir_all(&parent)?;
            }
            let path = parent.join(format!("{n:08}.bin"));
            if !path.exists() {
                fs::write(path, [42u8; 16])?;
            }
        }
        println!("生成 {count} 个实际文件: {}", fixture.display());
        return Ok(());
    }
    if args[0] != "scan" {
        bail!("未知命令");
    }
    let threads: usize = args[1].parse()?;
    let handle = scan::start(ScanRequest {
        roots: vec![fixture.canonicalize().context("请先 generate")?],
        exclusions: vec![],
        threads,
        database: root.join(format!(
            "scan-{threads}-{}.sqlite",
            if flat { "flat" } else { "tree" }
        )),
    });
    while let Ok(event) = handle.events.recv() {
        match event {
            ScanEvent::Finished {
                progress,
                cancelled,
            } => {
                println!(
                    "threads={threads} flat={flat} files={} directories={} elapsed_ms={} files_per_second={:.0} errors={} cancelled={cancelled}",
                    progress.files,
                    progress.directories,
                    progress.elapsed_ms,
                    progress.files as f64 * 1000.0 / progress.elapsed_ms.max(1) as f64,
                    progress.errors
                );
                break;
            }
            ScanEvent::Failed(error) => bail!("{error}"),
            _ => (),
        }
    }
    handle
        .join
        .join()
        .map_err(|_| anyhow::anyhow!("扫描线程异常"))?;
    Ok(())
}
