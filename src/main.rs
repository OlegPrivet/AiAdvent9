mod api;
mod cli;
mod config;
mod input;
mod repl;
mod settings;

use std::error::Error;
use std::io;
use std::process::ExitCode;

use api::NeuralDeepClient;
use clap::Parser;
use cli::Cli;
use config::Config;
use input::TerminalInput;
use settings::Settings;

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
    let client = NeuralDeepClient::new(config.api_key, config.base_url, config.model)?;
    let mut settings = Settings::default();

    let mut input = TerminalInput::new()?;
    let mut output = io::stdout();
    repl::run(
        &client,
        &mut settings,
        cli.question,
        &mut input,
        &mut output,
    )
    .await?;

    Ok(())
}
