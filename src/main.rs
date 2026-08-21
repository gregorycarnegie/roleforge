use roleforge::{Result, config, workflow};

fn main() -> Result<()> {
    config::load_dotenv(".env")?;
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    match std::env::args().nth(1).as_deref() {
        Some("pull") => workflow::pull(),
        Some("export") => workflow::export_latest(),
        Some("telegram-worker") => workflow::worker(),
        Some("run") => workflow::run(),
        Some("help") | Some("--help") | Some("-h") | None => {
            println!(
                "RoleForge — resume-aware job-market intelligence\n\nUsage:\n  roleforge pull\n  roleforge export\n  roleforge telegram-worker\n  roleforge run"
            );
            Ok(())
        }
        Some(command) => Err(format!("unknown command: {command}; run roleforge --help").into()),
    }
}
