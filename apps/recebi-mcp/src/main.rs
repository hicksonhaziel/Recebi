mod close_month;
mod config;
mod health;
mod mcp;
mod ptax;
mod qr;
mod receivable;
mod reconcile;
mod rpc;

use clap::Parser;
use close_month::CloseMonthService;
use config::AppConfig;
use ptax::HttpBcbPtax;

#[derive(Debug, Parser)]
#[command(name = "recebi-mcp", version, about = "Recebi local stdio MCP server")]
struct Cli {
    /// Trusted local configuration. MCP tool input cannot override it.
    #[arg(long)]
    config: std::path::PathBuf,
}

fn main() {
    let cli = Cli::parse();
    let config = match AppConfig::load(&cli.config) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("recebi-mcp configuration error: {error}");
            std::process::exit(2);
        }
    };
    let health = health::HealthService::new(config.clone());
    let receivables = match receivable::ReceivableService::new(config.clone()) {
        Ok(service) => service,
        Err(error) => {
            eprintln!("recebi-mcp storage error: {error}");
            std::process::exit(2);
        }
    };
    let reconciliation = match reconcile::ReconciliationService::live(config.clone()) {
        Ok(service) => service,
        Err(error) => {
            eprintln!("recebi-mcp reconciliation error: {error}");
            std::process::exit(2);
        }
    };
    let ptax = match HttpBcbPtax::new() {
        Ok(client) => client,
        Err(error) => {
            eprintln!("recebi-mcp PTAX configuration error: {error}");
            std::process::exit(2);
        }
    };
    let closing = match CloseMonthService::new(&config, ptax) {
        Ok(service) => service,
        Err(error) => {
            eprintln!("recebi-mcp monthly close error: {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = mcp::serve(&health, &receivables, &reconciliation, &closing) {
        eprintln!("recebi-mcp server error: {error}");
        std::process::exit(3);
    }
}
