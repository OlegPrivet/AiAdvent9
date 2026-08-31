mod api;
mod cli;
mod config;

use std::error::Error;
use std::process::ExitCode;

use api::NeuralDeepClient;
use clap::Parser;
use cli::Cli;
use config::Config;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Ошибка: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let config = Config::from_env()?;
    let client = NeuralDeepClient::new(
        config.api_key,
        config.base_url,
        config.model,
        config.max_tokens,
    )?;

    let answer = client.ask(&cli.question).await?;
    println!("{answer}");

    Ok(())
}
