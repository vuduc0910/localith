use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    // A missing .env file is fine; real environment variables win over it.
    let _ = dotenvy::dotenv();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match localith::cli::run(&args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("\n❌ Error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
