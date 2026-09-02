mod api;
mod chat;
mod cli;
mod config;
mod input;
mod repl;
mod settings;
mod ui;

use std::error::Error;
use std::io;
use std::process::ExitCode;

use api::NeuralDeepClient;
use chat::{Chat, ChatStore};
use clap::Parser;
use cli::Cli;
use config::Config;
use input::TerminalInput;
use ui::TerminalUi;

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
    let store = ChatStore::open()?;
    let mut chat = match cli.restore {
        Some(id) => store.load(id)?,
        None => Chat::new(),
    };

    let mut input = TerminalInput::new(cli.edit_mode)?;
    let ui = TerminalUi::detect();
    let mut output = io::stdout();
    repl::run(
        &client,
        &store,
        &mut chat,
        cli.question,
        &mut input,
        &mut output,
        &ui,
    )
    .await?;

    Ok(())
}
