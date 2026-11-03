fn main() {
    if let Err(error) = execute() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
fn execute() -> anyhow::Result<()> {
    // SAFETY: geteuid has no preconditions or memory arguments.
    if unsafe { libc::geteuid() } != 0 {
        anyhow::bail!("权限辅助程序必须通过系统授权启动");
    }
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 1 || args[0].len() > 16384 {
        anyhow::bail!("无效请求");
    }
    let request: rsclear::system::PrivilegedRequest = serde_json::from_str(&args[0])?;
    // Never use PATH or user configuration to choose a privileged executable.
    let result = rsclear::system::execute(&request, &rsclear::model::Cancellation::default())?;
    println!("{result}");
    Ok(())
}
