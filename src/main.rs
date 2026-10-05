mod agent;
mod agent_catalog;
mod agents_ui;
mod api;
mod chat;
mod cli;
mod config;
mod context;
mod input;
mod invariants;
mod llm;
#[cfg(test)]
mod llm_tests;
mod llm_ui;
mod mcp;
mod mcp_demo;
mod mcp_ui;
mod memory;
mod metrics;
mod pricing;
mod rag;
mod rag_answer;
mod rag_chat;
mod rag_chunk;
mod rag_cli;
mod rag_day23;
mod rag_day24;
mod rag_day25;
mod rag_eval;
mod rag_extract;
mod rag_pipeline;
mod repl;
mod settings;
mod summary;
mod task;
#[cfg(test)]
mod test_http;
mod tui;
mod ui;

use std::error::Error;
use std::io;
use std::process::ExitCode;

use agent::{Agent, AgentRequest};
use api::NeuralDeepClient;
use chat::{Chat, ChatStore};
use clap::Parser;
use cli::Cli;

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
    if cli.mcp_demo_server {
        return mcp_demo::run().await;
    }
    if let Some(command) = cli.command {
        match command {
            cli::RootCommand::Llm { command } => {
                let store = ChatStore::open()?;
                if let llm::LlmCommand::Ask { question, profile } = command {
                    let selected = match profile {
                        Some(id) => store.llms().get(&id)?,
                        None => store.llms().active()?,
                    };
                    let mut chat = Chat::new();
                    chat.settings_mut().apply_profile(selected);
                    let client = Agent::new(NeuralDeepClient::new(
                        String::new(),
                        config::DEFAULT_BASE_URL.into(),
                    )?);
                    let request =
                        AgentRequest::new(&chat, question.clone(), store.agents().list()?);
                    let answer = client.respond_streaming(request, |_| Ok(())).await?;
                    println!("{}\n\nВремя: {} мс", answer.content, answer.elapsed_ms);
                    return Ok(());
                }
                println!("{}", llm::command(&store.llms(), command)?);
                return Ok(());
            }
            command => return rag_cli::run(command).await.map_err(Into::into),
        }
    }
    let client = Agent::new(NeuralDeepClient::new(
        String::new(),
        config::DEFAULT_BASE_URL.into(),
    )?);
    let store = ChatStore::open()?;
    let mut chat = match cli.restore {
        Some(id) => store.load(id)?,
        None => Chat::new(),
    };

    llm::sync_chat(&store, &mut chat)?;
    if tui::is_supported() {
        tui::run(&client, &store, &mut chat, cli.question, cli.edit_mode).await?;
    } else {
        let prices = if store.llms().active()?.provider == llm::Provider::NeuralDeep {
            PriceCatalog::fetch().await.unwrap_or_default()
        } else {
            PriceCatalog::default()
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
            repl::ReplDisplay {
                ui: &ui,
                prices: &prices,
            },
        )
        .await?;
    }

    Ok(())
}
