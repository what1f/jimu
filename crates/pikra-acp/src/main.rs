#[tokio::main]
async fn main() -> std::process::ExitCode {
    match pikra_acp::args::run(std::env::args().skip(1)).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
