mod api;
mod chat;
mod cli;
mod config;
mod input;
mod metrics;
mod pricing;
mod repl;
mod settings;
mod tui;
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
use pricing::PriceCatalog;
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
    let client = NeuralDeepClient::new(config.api_key, config.base_url)?;
    let store = ChatStore::open()?;
    let mut chat = match cli.restore {
        Some(id) => store.load(id)?,
        None => Chat::new(),
    };

    if tui::is_supported() {
        tui::run(&client, &store, &mut chat, cli.question, cli.edit_mode).await?;
    } else {
        let prices = PriceCatalog::fetch().await.unwrap_or_default();
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
            repl::ReplDisplay {
                ui: &ui,
                prices: &prices,
            },
        )
        .await?;
    }

    Ok(())
}
