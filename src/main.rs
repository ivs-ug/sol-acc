use base64ct::{Base64, Encoding};
use anyhow::Result;
use clap::{Parser, Subcommand};
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::{CommitmentConfig, RpcAccountInfoConfig, RpcProgramAccountsConfig, UiAccountEncoding};
use solana_address_lookup_table_interface::state::AddressLookupTable;
use solana_client::rpc_response::UiAccount;
use solana_pubkey::Pubkey;
use std::str::FromStr;
use std::time::Duration;
use serde_json::json;

#[derive(Parser)]
#[command(name = "sol-acc")]
#[command(about = "Solana account fetching tool")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Fetch program accounts
    Accs {
        /// Program address to fetch accounts from
        program: String,

        /// RPC node URL (default: solana-rpc.publicnode.com)
        #[arg(short, long, env = "RPC_NODE", default_value = "https://solana-rpc.publicnode.com")]
        url: String,

        /// Account type parser (alt, data)
        #[arg(short = 't', long)]
        parser: Option<String>,

        /// Data range as offset:size (e.g., 10:30)
        #[arg(short, long)]
        data: Option<String>,

        /// Filter by data at offset (e.g., 10:0x0f0000 or 10:Pubkey58)
        #[arg(short, long)]
        filter: Vec<String>,

        /// Output JSON file (omit for stdout)
        #[arg(short, long)]
        output: Option<String>,
    },
}

trait AccountDecoder: Send + Sync {
    fn decode(&self, data: &[u8]) -> Result<serde_json::Value>;
}

struct AltDecoder;
impl AccountDecoder for AltDecoder {
    fn decode(&self, data: &[u8]) -> Result<serde_json::Value> {
        let alt = AddressLookupTable::deserialize(data)?;
        let addresses: Vec<String> = alt.addresses
            .as_ref()
            .iter()
            .map(|pk| pk.to_string())
            .collect();
        
        Ok(json!({
            "type": "address_lookup_table",
            "addresses": addresses,
            "num_addresses": addresses.len(),
        }))
    }
}

struct DataDecoder {
    offset: usize,
    size: usize,
}

impl AccountDecoder for DataDecoder {
    fn decode(&self, data: &[u8]) -> Result<serde_json::Value> {
        let end = (self.offset + self.size).min(data.len());
        let slice = &data[self.offset..end];
        
        Ok(json!({
            "type": "raw_data",
            "offset": self.offset,
            "size": self.size,
            "hex": hex::encode(slice),
            "base64": Base64::encode_string(slice),
        }))
    }
}

fn get_decoder(parser: Option<&str>, data_range: Option<&str>) -> Result<Box<dyn AccountDecoder>> {
    if let Some(range) = data_range {
        let parts: Vec<&str> = range.split(':').collect();
        anyhow::ensure!(parts.len() == 2, "Data range must be offset:size");
        let offset = parts[0].parse()?;
        let size = parts[1].parse()?;
        return Ok(Box::new(DataDecoder { offset, size }));
    }

    match parser {
        Some("alt") => Ok(Box::new(AltDecoder)),
        Some(p) => anyhow::bail!("Unknown parser: {}", p),
        None => anyhow::bail!("Must specify either --parser or --data"),
    }
}

fn parse_filter(filter: &str) -> Result<(usize, Vec<u8>)> {
    let parts: Vec<&str> = filter.split(':').collect();
    anyhow::ensure!(parts.len() == 2, "Filter must be offset:data");
    
    let offset: usize = parts[0].parse()?;
    let data_str = parts[1];
    
    let bytes = if data_str.starts_with("0x") {
        hex::decode(&data_str[2..])?
    } else {
        // Try base58 (pubkey)
        let pubkey = Pubkey::from_str(data_str)?;
        pubkey.to_bytes().to_vec()
    };
    
    Ok((offset, bytes))
}

fn matches_filters(data: &[u8], filters: &[(usize, Vec<u8>)]) -> bool {
    for (offset, pattern) in filters {
        let end = offset + pattern.len();
        if end > data.len() {
            return false;
        }
        if &data[*offset..end] != pattern.as_slice() {
            return false;
        }
    }
    true
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Accs { program, url, parser, data, filter, output } => {
            let rpc = RpcClient::new_with_timeout_and_commitment(
                url,
                Duration::from_secs(15 * 60),
                CommitmentConfig::processed(),
            );

            let program_pubkey = Pubkey::from_str(&program)?;
            
            let cfg = RpcProgramAccountsConfig {
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64Zstd),
                    ..Default::default()
                },
                ..Default::default()
            };

            let accounts: Vec<(Pubkey, UiAccount)> = rpc
                .get_program_ui_accounts_with_config(&program_pubkey, cfg)?;

            eprintln!("Fetched {} accounts", accounts.len());

            let decoder = get_decoder(parser.as_deref(), data.as_deref())?;
            
            let filters: Result<Vec<_>> = filter.iter()
                .map(|f| parse_filter(f))
                .collect();
            let filters = filters?;

            let mut results = Vec::new();
            let mut processed = 0;
            let mut filtered_out = 0;

            for (pubkey, acc) in accounts {
                let Some(data) = acc.data.decode() else {
                    continue;
                };

                if !matches_filters(&data, &filters) {
                    filtered_out += 1;
                    continue;
                }

                match decoder.decode(&data) {
                    Ok(decoded) => {
                        results.push(json!({
                            "pubkey": pubkey.to_string(),
                            "lamports": acc.lamports,
                            "owner": acc.owner,
                            "data": decoded,
                        }));
                        processed += 1;
                    }
                    Err(e) => {
                        eprintln!("Failed to decode {}: {}", pubkey, e);
                    }
                }
            }

            eprintln!("Processed: {}, Filtered: {}", processed, filtered_out);

            let output_json = json!({
                "program": program,
                "count": processed,
                "accounts": results,
            });

            if let Some(file) = output {
                std::fs::write(&file, serde_json::to_string_pretty(&output_json)?)?;
                eprintln!("Saved to {}", file);
            } else {
                println!("{}", serde_json::to_string_pretty(&output_json)?);
            }

            Ok(())
        }
    }
}