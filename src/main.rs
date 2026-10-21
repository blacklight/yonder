#[tokio::main]
async fn main() -> std::process::ExitCode {
    match yonder::cli::main().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
