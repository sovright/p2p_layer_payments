//! `paid-services-mock`: runs the mock node and drives it, for the demo. Test data only.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{value_parser, Arg, ArgMatches, Command};
use paid_services_mock::{
    http, MockIssuer, MockNode, MockParams, TEST_CLIENT_NODE_ID, TEST_PROVIDER_NODE_ID,
};
use paid_services_types::config::SidecarConfig;
use paid_services_types::expiry::LIBRUSTZCASH_DEFAULT;
use rand_core::{OsRng, RngCore};
use serde_json::{json, Value};

const NOTICE: &str = "SIMULATION ONLY: a mock node and a mock issuer with test data. \
No wallet, no Zakura node and no payment are involved.";

fn cli() -> Command {
    let config = || {
        Arg::new("config")
            .long("config")
            .value_name("FILE")
            .required(true)
            .value_parser(value_parser!(PathBuf))
    };
    Command::new("paid-services-mock")
        .about(NOTICE)
        .subcommand_required(true)
        .subcommand(
            Command::new("node")
                .about("Run a mock node and issuer; writes a cookie and a sidecar config into a new directory")
                .arg(
                    Arg::new("dir")
                        .long("dir")
                        .value_name("NEW_DIR")
                        .required(true)
                        .value_parser(value_parser!(PathBuf)),
                )
                .arg(
                    Arg::new("port")
                        .long("port")
                        .value_name("PORT")
                        .value_parser(value_parser!(u16)),
                )
                .arg(
                    Arg::new("serve-secs")
                        .long("serve-secs")
                        .value_name("SECS")
                        .value_parser(value_parser!(u64)),
                ),
        )
        .subcommand(
            Command::new("control")
                .about("Drive a running mock node (test data)")
                .arg(config())
                .subcommand_required(true)
                .subcommand(
                    Command::new("pay")
                        .about("Record a mock payment in the next block")
                        .arg(Arg::new("purchase").value_name("PURCHASE_ID").required(true))
                        .arg(
                            Arg::new("amount")
                                .long("amount")
                                .value_name("ZAT")
                                .value_parser(value_parser!(u64)),
                        )
                        .arg(
                            Arg::new("expiry")
                                .long("expiry")
                                .value_name("HEIGHT")
                                .value_parser(value_parser!(u32)),
                        ),
                )
                .subcommand(
                    Command::new("mine").arg(
                        Arg::new("blocks")
                            .value_name("BLOCKS")
                            .required(true)
                            .value_parser(value_parser!(u32)),
                    ),
                )
                .subcommand(
                    Command::new("drop-next")
                        .arg(Arg::new("method").value_name("METHOD").required(true)),
                )
                .subcommand(
                    Command::new("tamper").arg(Arg::new("mode").value_name("MODE").required(true)),
                ),
        )
}

fn main() {
    let matches = cli().try_get_matches().unwrap_or_else(|e| e.exit());
    let code = match matches.subcommand() {
        Some(("node", m)) => node(m),
        Some(("control", m)) => control(m),
        _ => 2,
    };
    std::process::exit(code);
}

fn create_new(path: &Path, contents: &str) -> std::io::Result<()> {
    let mut f = OpenOptions::new().write(true).create_new(true).open(path)?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()
}

/// Starts the mock node and writes a fresh cookie and sidecar config. Never overwrites a file.
fn start(dir: &Path, port: u16) -> Result<(MockNode, PathBuf), String> {
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut secret = [0u8; 16];
    OsRng
        .try_fill_bytes(&mut secret)
        .map_err(|e| e.to_string())?;
    let cookie = format!("__cookie__:{}", hex::encode(secret));
    let issuer = MockIssuer::new(MockParams::default());
    let node = MockNode::spawn(
        issuer.clone(),
        TEST_CLIENT_NODE_ID,
        &cookie,
        SocketAddr::from(([127, 0, 0, 1], port)),
        true,
    )
    .map_err(|e| format!("cannot start the mock node: {e}"))?;
    let cookie_path = dir.join("node.cookie");
    create_new(&cookie_path, &cookie)
        .map_err(|e| format!("{}: {e} (use a new directory)", cookie_path.display()))?;
    let cfg = SidecarConfig {
        node_rpc: node.addr,
        cookie_file: PathBuf::from("node.cookie"),
        state_file: PathBuf::from("sidecar-state.json"),
        network: issuer.network(),
        provider_node_id: hex::encode(TEST_PROVIDER_NODE_ID),
        issuer_public_key: hex::encode(issuer.public_key()),
        purchase_limit_zat: 100_000,
        wallet_expiry_profile: LIBRUSTZCASH_DEFAULT.name.to_string(),
        offer_margin_blocks: 10,
    };
    let cfg_path = dir.join("sidecar.json");
    let text = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())? + "\n";
    create_new(&cfg_path, &text)
        .map_err(|e| format!("{}: {e} (use a new directory)", cfg_path.display()))?;
    Ok((node, cfg_path))
}

// The CLI's own output.
#[allow(clippy::print_stdout, clippy::print_stderr)]
fn node(m: &ArgMatches) -> i32 {
    let Some(dir) = m.get_one::<PathBuf>("dir") else {
        return 2;
    };
    let port = m.get_one::<u16>("port").copied().unwrap_or(0);
    eprintln!("{NOTICE}\n");
    let (node, cfg_path) = match start(dir, port) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    println!(
        "mock node listening on {} (loopback, cookie auth)",
        node.addr
    );
    println!("sidecar config: {}", cfg_path.display());
    println!(
        "offers (test data): {} relay-10 (25000 zat, 10 blocks), relay-20 (40000 zat, 20 blocks)",
        paid_services_relay::SERVICE_KIND
    );
    let _ = std::io::stdout().flush();
    match m.get_one::<u64>("serve-secs") {
        Some(s) => std::thread::sleep(Duration::from_secs(*s)),
        None => loop {
            std::thread::park();
        },
    }
    0
}

fn control_request(m: &ArgMatches) -> Option<(&'static str, Value)> {
    match m.subcommand()? {
        ("pay", p) => {
            let mut params = json!({"purchase_id": p.get_one::<String>("purchase")?});
            if let Some(a) = p.get_one::<u64>("amount") {
                params["amount_zat"] = json!(a);
            }
            if let Some(h) = p.get_one::<u32>("expiry") {
                params["expiry_height"] = json!(h);
            }
            Some(("mock_pay", params))
        }
        ("mine", p) => Some(("mock_mine", json!({"blocks": p.get_one::<u32>("blocks")?}))),
        ("drop-next", p) => Some((
            "mock_drop_next",
            json!({"method": p.get_one::<String>("method")?}),
        )),
        ("tamper", p) => Some(("mock_tamper", json!({"mode": p.get_one::<String>("mode")?}))),
        _ => None,
    }
}

// The CLI's own output.
#[allow(clippy::print_stdout, clippy::print_stderr)]
fn control(m: &ArgMatches) -> i32 {
    let (Some(config), Some((method, params))) =
        (m.get_one::<PathBuf>("config"), control_request(m))
    else {
        return 2;
    };
    let cfg: SidecarConfig = match fs::read_to_string(config)
        .map_err(|e| e.to_string())
        .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string()))
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {}: {e}", config.display());
            return 2;
        }
    };
    let cookie_path = if cfg.cookie_file.is_relative() {
        config
            .parent()
            .unwrap_or(Path::new("."))
            .join(&cfg.cookie_file)
    } else {
        cfg.cookie_file.clone()
    };
    let result = fs::read_to_string(&cookie_path)
        .map_err(|e| format!("{}: {e}", cookie_path.display()))
        .and_then(|cookie| http::call(cfg.node_rpc, &cookie, method, params));
    match result {
        Ok(v) => {
            println!("{method} (mock control, test data): {v}");
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            5
        }
    }
}
