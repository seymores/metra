mod cli;
mod output;
mod quic;
mod rest;
mod transfer;
mod tui;

use anyhow::Result;
use clap::Parser;
use reqwest::Client;

use crate::{
    cli::{Cli, Command, TransferAction},
    output::print_output,
};

#[tokio::main]
async fn main() -> Result<()> {
    transfer::install_crypto_provider();
    let cli = Cli::parse();
    let http = Client::new();

    match cli.command.unwrap_or(Command::Tui) {
        Command::Tui => tui::run_tui(&http, &cli.server).await,
        Command::Health => {
            let health = rest::fetch_health(&http, &cli.server).await?;
            print_output(&health, cli.output)?;
            Ok(())
        }
        Command::Transfer { action } => match action {
            TransferAction::Create(args) => {
                let request = transfer::create_transfer_request(&args)?;
                let created = rest::create_transfer(&http, &cli.server, &request).await?;
                print_output(&created, cli.output)?;
                Ok(())
            }
            TransferAction::Status(args) => {
                let transfer =
                    rest::fetch_transfer_status(&http, &cli.server, args.transfer_id).await?;
                print_output(&transfer, cli.output)?;
                Ok(())
            }
            TransferAction::Send(args) => {
                let report = transfer::send_transfer(&http, &cli.server, args).await?;
                print_output(&report, cli.output)?;
                Ok(())
            }
            TransferAction::Bench(args) => {
                let report = transfer::run_benchmark(&http, &cli.server, args).await?;
                print_output(&report, cli.output)?;
                Ok(())
            }
            TransferAction::Matrix(args) => {
                let report = transfer::run_benchmark_matrix(&http, &cli.server, args).await?;
                print_output(&report, cli.output)?;
                Ok(())
            }
            TransferAction::Compare(args) => {
                let report = transfer::run_benchmark_compare(&http, &cli.server, args).await?;
                print_output(&report, cli.output)?;
                Ok(())
            }
            TransferAction::CompareSeries(args) => {
                let report =
                    transfer::run_benchmark_compare_series(&http, &cli.server, args).await?;
                print_output(&report, cli.output)?;
                Ok(())
            }
        },
    }
}
