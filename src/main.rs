//! `paid-stream-bridge`: a simulation-only CLI. See README.md.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use paid_stream_bridge::bridge::{
    random_bytes, Bridge, BridgeError, Config, PaymentRequest, PurchaseView, SIMULATION_NOTICE,
};
use paid_stream_bridge::expiry::LIBRUSTZCASH_DEFAULT;
use paid_stream_bridge::hexutil;
use paid_stream_bridge::mock::{
    MockIssuer, MockNode, MockParams, TEST_CLIENT_NODE_ID, TEST_PROVIDER_NODE_ID,
};
use paid_stream_bridge::rpc::RpcClient;
use paid_stream_bridge::uri::format_zec;

const USAGE: &str = "\
paid-stream-bridge (SIMULATION ONLY: mock node and mock issuer; no wallet, no payment)

bridge commands:
  paid-stream-bridge --config FILE buy SERVICE
  paid-stream-bridge --config FILE status [PURCHASE]
  paid-stream-bridge --config FILE resume
  paid-stream-bridge --config FILE show PURCHASE

mock commands:
  paid-stream-bridge mock-node --dir NEW_DIR [--port PORT] [--serve-secs SECS]
  paid-stream-bridge mock-control --config FILE pay PURCHASE_ID [--amount ZAT] [--expiry HEIGHT]
  paid-stream-bridge mock-control --config FILE mine BLOCKS
  paid-stream-bridge mock-control --config FILE drop-next METHOD
  paid-stream-bridge mock-control --config FILE tamper MODE

PURCHASE is at least 8 hex characters of a purchase_id or request_id.";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(run(&args));
}

fn usage_error(msg: &str) -> i32 {
    eprintln!("error: {msg}\n\n{USAGE}");
    2
}

fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        None => usage_error("no command"),
        Some("-h" | "--help") => {
            println!("{USAGE}");
            0
        }
        Some("mock-node") => mock_node(&args[1..]),
        Some("mock-control") => mock_control(&args[1..]),
        Some("--config") => bridge_command(&args[1..]),
        Some(other) => usage_error(&format!("unknown argument {other:?}")),
    }
}

fn bridge_command(args: &[String]) -> i32 {
    eprintln!("{SIMULATION_NOTICE}\n");
    let (config_path, rest) = match args {
        [path, rest @ ..] => (PathBuf::from(path), rest),
        [] => return usage_error("--config needs a file"),
    };
    let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
    let cfg = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    // Parse fully before taking the lock, so a bad command line changes nothing.
    enum Cmd<'a> {
        Buy(&'a str),
        Status(Option<&'a str>),
        Resume,
        Show(&'a str),
    }
    let cmd = match rest.as_slice() {
        ["buy", service] => Cmd::Buy(service),
        ["status"] => Cmd::Status(None),
        ["status", which] if !which.starts_with('-') => Cmd::Status(Some(which)),
        ["resume"] => Cmd::Resume,
        ["show", which] if !which.starts_with('-') => Cmd::Show(which),
        _ => return usage_error(&format!("cannot parse {:?}", rest.join(" "))),
    };
    let mut bridge = match Bridge::open(cfg) {
        Ok(b) => b,
        Err(e) => return fail(&e),
    };
    let result = match cmd {
        Cmd::Buy(service) => bridge.buy(service).map(|v| vec![v]),
        Cmd::Status(which) => bridge.status(which),
        Cmd::Resume => bridge.resume(),
        Cmd::Show(which) => bridge.show(which).map(|v| vec![v]),
    };
    match result {
        Ok(views) => {
            if views.is_empty() {
                println!("no open purchases");
            }
            for v in &views {
                print_view(v);
            }
            0
        }
        Err(e) => fail(&e),
    }
}

fn fail(e: &BridgeError) -> i32 {
    eprintln!("error: {e}");
    e.exit_code()
}

fn print_view(v: &PurchaseView) {
    println!(
        "purchase {} (request {})",
        v.purchase_id.as_deref().unwrap_or("-"),
        v.request_id
    );
    println!(
        "  service {}  stage {}  provider status {}",
        v.service,
        v.stage.as_str(),
        v.last_status.as_deref().unwrap_or("-")
    );
    match &v.payment {
        Some(PaymentRequest::Offered {
            uri,
            amount_zat,
            deadline_height,
            expected_expiry,
            reshown,
        }) => {
            println!("  payment request (simulation; do not open in a wallet, do not pay):");
            println!("    {uri}");
            println!(
                "    amount {} ZEC, deadline height {deadline_height} (test data)",
                format_zec(*amount_zat)
            );
            println!(
                "    a wallet paying now would set nExpiryHeight about {expected_expiry} (expiry profile; \
a heuristic, not a guarantee: paying later can miss the deadline rule)"
            );
            if *reshown {
                println!(
                    "    shown again: if you already paid this request, do not pay again. A payment the provider \
has not seen yet is still reported as Pending, and a second payment only goes to reconciliation."
                );
            }
        }
        Some(PaymentRequest::Withheld(reason)) => println!("  no payment request: {reason}"),
        None => {}
    }
    if let Some(i) = &v.install {
        println!(
            "  grant installed for blocks [{}, {}) (mock install; no stream is consumed)",
            i.start_height, i.end_height
        );
    }
    if let Some(n) = &v.note {
        println!("  note: {n}");
    }
}

fn take_flag(args: &mut Vec<&str>, flag: &str) -> Result<Option<String>, String> {
    match args.iter().position(|a| *a == flag) {
        None => Ok(None),
        Some(i) if i + 1 < args.len() => {
            let v = args.remove(i + 1).to_string();
            args.remove(i);
            Ok(Some(v))
        }
        Some(_) => Err(format!("{flag} needs a value")),
    }
}

fn create_new(path: &Path, contents: &str) -> std::io::Result<()> {
    let mut f = OpenOptions::new().write(true).create_new(true).open(path)?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()
}

fn parse_mock_node_args(args: &[String]) -> Result<(PathBuf, u16, Option<u64>), String> {
    let mut args: Vec<&str> = args.iter().map(String::as_str).collect();
    let dir = take_flag(&mut args, "--dir")?.ok_or("--dir is required")?;
    let port = take_flag(&mut args, "--port")?
        .map(|p| p.parse::<u16>().map_err(|e| format!("--port: {e}")))
        .transpose()?
        .unwrap_or(0);
    let secs = take_flag(&mut args, "--serve-secs")?
        .map(|s| s.parse::<u64>().map_err(|e| format!("--serve-secs: {e}")))
        .transpose()?;
    if !args.is_empty() {
        return Err(format!("unexpected arguments {args:?}"));
    }
    Ok((PathBuf::from(dir), port, secs))
}

/// Starts the mock node and writes a fresh cookie and bridge config. Never overwrites a file.
fn start_mock_node(dir: &Path, port: u16) -> Result<(MockNode, PathBuf), String> {
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let cookie = format!(
        "__cookie__:{}",
        hexutil::encode(&random_bytes::<16>().map_err(|e| e.to_string())?)
    );
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
    let cfg = Config {
        node_rpc: node.addr,
        cookie_file: PathBuf::from("node.cookie"),
        state_file: PathBuf::from("bridge-state.json"),
        network: issuer.network(),
        provider_node_id: hexutil::encode(&TEST_PROVIDER_NODE_ID),
        issuer_public_key: hexutil::encode(&issuer.public_key()),
        purchase_limit_zat: 100_000,
        wallet_expiry_profile: LIBRUSTZCASH_DEFAULT.name.to_string(),
        offer_margin_blocks: 10,
    };
    let cfg_path = dir.join("bridge.json");
    let text = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())? + "\n";
    create_new(&cfg_path, &text)
        .map_err(|e| format!("{}: {e} (use a new directory)", cfg_path.display()))?;
    Ok((node, cfg_path))
}

fn mock_node(args: &[String]) -> i32 {
    let (dir, port, secs) = match parse_mock_node_args(args) {
        Ok(x) => x,
        Err(e) => return usage_error(&e),
    };
    eprintln!("{SIMULATION_NOTICE}\n");
    let (node, cfg_path) = match start_mock_node(&dir, port) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let addr = node.addr;
    println!("mock node listening on {addr} (loopback, cookie auth)");
    println!("bridge config: {}", cfg_path.display());
    println!("services (test data): synthetic-10 (25000 zat, 10 blocks), synthetic-20 (40000 zat, 20 blocks)");
    let _ = std::io::stdout().flush();
    match secs {
        Some(s) => std::thread::sleep(Duration::from_secs(s)),
        None => loop {
            std::thread::park();
        },
    }
    0
}

fn parse_mock_control(args: &[String]) -> Result<(String, &'static str, Value), String> {
    let mut args: Vec<&str> = args.iter().map(String::as_str).collect();
    let config = take_flag(&mut args, "--config")?.ok_or("mock-control needs --config")?;
    let amount = take_flag(&mut args, "--amount")?;
    let expiry = take_flag(&mut args, "--expiry")?;
    let (method, params) = match args.as_slice() {
        ["pay", pid] => {
            let mut params = json!({"purchase_id": pid});
            if let Some(a) = amount {
                params["amount_zat"] =
                    json!(a.parse::<u64>().map_err(|e| format!("--amount: {e}"))?);
            }
            if let Some(h) = expiry {
                params["expiry_height"] =
                    json!(h.parse::<u32>().map_err(|e| format!("--expiry: {e}"))?);
            }
            ("mock_pay", params)
        }
        ["mine", n] => (
            "mock_mine",
            json!({"blocks": n.parse::<u32>().map_err(|e| format!("blocks: {e}"))?}),
        ),
        ["drop-next", method] => ("mock_drop_next", json!({"method": method})),
        ["tamper", mode] => ("mock_tamper", json!({"mode": mode})),
        _ => return Err(format!("cannot parse {:?}", args.join(" "))),
    };
    Ok((config, method, params))
}

fn mock_control(args: &[String]) -> i32 {
    let (config, method, params) = match parse_mock_control(args) {
        Ok(x) => x,
        Err(e) => return usage_error(&e),
    };
    let cfg = match Config::load(Path::new(&config)) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let client = match RpcClient::from_cookie_file(cfg.node_rpc, &cfg.cookie_file) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    match client.call(method, params) {
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
