use anyhow::Result;
use base64ct::{Base64, Encoding};
use clap::{Parser, Subcommand};
use serde_json::json;
use solana_address_lookup_table_interface::state::AddressLookupTable;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::{
    CommitmentConfig, RpcAccountInfoConfig, RpcProgramAccountsConfig, UiAccountEncoding,
    UiDataSliceConfig,
};
use solana_client::rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType};
use solana_client::rpc_response::UiAccount;
use solana_pubkey::Pubkey;
use std::str::FromStr;
use std::time::Duration;

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
        #[arg(
            short,
            long,
            env = "RPC_NODE",
            default_value = "https://solana-rpc.publicnode.com"
        )]
        url: String,

        /// Account type parser (alt) - only works without -d
        #[arg(short = 't', long, conflicts_with = "data")]
        parser: Option<String>,

        /// Data slice as offset:size (e.g., 10:30) - conflicts with -t
        #[arg(short, long, conflicts_with = "parser")]
        data: Option<String>,

        /// Filter by data at offset (e.g., 10:0x0f0000 or 10:Pubkey58)
        #[arg(short, long)]
        filter: Vec<String>,

        /// Filter by account data size
        #[arg(short, long)]
        size: Option<u64>,

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
        let addresses: Vec<String> = alt
            .addresses
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

fn get_decoder(parser: Option<&str>) -> Result<Option<Box<dyn AccountDecoder>>> {
    match parser {
        Some("alt") => Ok(Some(Box::new(AltDecoder))),
        Some(p) => anyhow::bail!("Unknown parser: {}", p),
        None => Ok(None),
    }
}

fn parse_filter(filter: &str) -> Result<RpcFilterType> {
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

    Ok(RpcFilterType::Memcmp(Memcmp::new(
        offset,
        MemcmpEncodedBytes::Bytes(bytes),
    )))
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Accs {
            program,
            url,
            parser,
            data,
            filter,
            size,
            output,
        } => {
            let rpc = RpcClient::new_with_timeout_and_commitment(
                url,
                Duration::from_secs(15 * 60),
                CommitmentConfig::processed(),
            );

            let program_pubkey = Pubkey::from_str(&program)?;

            // Parse data slice if provided
            let data_slice = if let Some(range) = &data {
                let parts: Vec<&str> = range.split(':').collect();
                anyhow::ensure!(parts.len() == 2, "Data range must be offset:size");
                let offset: usize = parts[0].parse()?;
                let length: usize = parts[1].parse()?;
                Some(UiDataSliceConfig { offset, length })
            } else {
                None
            };

            // Parse filters
            let mut rpc_filters: Vec<RpcFilterType> = filter
                .iter()
                .map(|f| parse_filter(f))
                .collect::<Result<Vec<_>>>()?;

            // Add size filter if provided
            if let Some(size_val) = size {
                rpc_filters.push(RpcFilterType::DataSize(size_val));
            }

            let cfg = RpcProgramAccountsConfig {
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64Zstd),
                    data_slice,
                    ..Default::default()
                },
                filters: if rpc_filters.is_empty() {
                    None
                } else {
                    Some(rpc_filters)
                },
                ..Default::default()
            };

            let accounts: Vec<(Pubkey, UiAccount)> =
                rpc.get_program_ui_accounts_with_config(&program_pubkey, cfg)?;

            eprintln!("Fetched {} accounts", accounts.len());

            let decoder = get_decoder(parser.as_deref())?;
            let mut results = Vec::new();
            let mut processed = 0;

            for (pubkey, acc) in accounts {
                let data_value = if let Some(ref dec) = decoder {
                    // Full decode if parser specified
                    let Some(data) = acc.data.decode() else {
                        continue;
                    };
                    match dec.decode(&data) {
                        Ok(decoded) => decoded,
                        Err(e) => {
                            eprintln!("Failed to decode {}: {}", pubkey, e);
                            continue;
                        }
                    }
                } else {
                    // Raw data output
                    let Some(data) = acc.data.decode() else {
                        continue;
                    };
                    json!({
                        "type": "raw",
                        "hex": hex::encode(&data),
                        "base64": Base64::encode_string(&data),
                        "size": data.len(),
                    })
                };

                results.push(json!({
                    "pubkey": pubkey.to_string(),
                    "lamports": acc.lamports,
                    "owner": acc.owner,
                    "data": data_value,
                }));
                processed += 1;
            }

            eprintln!("Processed: {}", processed);

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