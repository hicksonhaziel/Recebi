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
use recebi_store::ReceivableStore;

#[derive(Debug, Parser)]
#[command(name = "recebi-mcp", version, about = "Recebi local stdio MCP server")]
struct Cli {
    /// Trusted local configuration. MCP tool input cannot override it.
    #[arg(long)]
    config: std::path::PathBuf,
    /// Verify the local ledger offline, print the material root, and exit.
    ///
    /// Performs no network call and starts no MCP server. Intended for restore
    /// drills and incident triage.
    #[arg(long)]
    verify_ledger: bool,
}

/// Verifies the event hash chain, material-table root, and checkpoint chain of
/// the configured local ledger without any network access.
fn verify_ledger(config: &AppConfig) -> ! {
    let store = match ReceivableStore::open(config.database_path()) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("recebi-mcp ledger storage error: {error}");
            std::process::exit(4);
        }
    };
    if let Err(error) = store.verify_event_chain() {
        eprintln!("recebi-mcp event chain error: {error}");
        std::process::exit(4);
    }
    let root = match store.ledger_fingerprint() {
        Ok(root) => root,
        Err(error) => {
            eprintln!("recebi-mcp ledger integrity error: {error}");
            std::process::exit(4);
        }
    };
    let root_hex = root.iter().fold(String::new(), |mut hex, byte| {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
        hex
    });
    println!(
        "{}",
        serde_json::json!({
            "status": "ok",
            "event_chain": "verified",
            "ledger_checkpoints": "verified",
            "material_ledger_root": root_hex,
            "cluster": config.recebi.cluster.as_str(),
            "network_checks": "not_run",
            "custody": "none",
        })
    );
    std::process::exit(0);
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
    if cli.verify_ledger {
        verify_ledger(&config);
    }
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
