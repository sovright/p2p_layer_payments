//! `paid-services-sidecar`: a simulation-only CLI. See the repository README.

use std::path::PathBuf;

use clap::{value_parser, Arg, ArgMatches, Command};
use paid_services_sidecar::bridge::{
    load_config, Bridge, BridgeError, PaymentRequest, PurchaseView, SIMULATION_NOTICE,
};
use paid_services_types::uri::format_zec;

fn cli() -> Command {
    let purchase = || {
        Arg::new("purchase")
            .value_name("PURCHASE")
            .help("At least 8 hex characters of a purchase_id or request_id")
    };
    Command::new("paid-services-sidecar")
        .about("SIMULATION ONLY: buys paid services from a mock node and a mock issuer. No wallet, no payment.")
        .arg(
            Arg::new("config")
                .long("config")
                .value_name("FILE")
                .required(true)
                .value_parser(value_parser!(PathBuf)),
        )
        .subcommand_required(true)
        .subcommand(
            Command::new("buy")
                .about("Start a purchase, or continue the open one for SERVICE")
                .arg(Arg::new("service").value_name("SERVICE").required(true)),
        )
        .subcommand(
            Command::new("status")
                .about("Ask the provider, through the node, for the status of open purchases")
                .arg(purchase()),
        )
        .subcommand(Command::new("resume").about("Finish what a lost answer or a restart left open"))
        .subcommand(
            Command::new("show")
                .about("Show a still-pending payment request again")
                .arg(purchase().required(true)),
        )
}

fn main() {
    // A bad command line exits here with code 2, before the state lock is taken.
    let matches = cli().try_get_matches().unwrap_or_else(|e| e.exit());
    std::process::exit(run(&matches));
}

fn run(matches: &ArgMatches) -> i32 {
    notice();
    let Some(config_path) = matches.get_one::<PathBuf>("config") else {
        return fail(&BridgeError::Usage("--config is required".into()));
    };
    let cfg = match load_config(config_path) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let mut bridge = match Bridge::open(cfg) {
        Ok(b) => b,
        Err(e) => return fail(&e),
    };
    let arg = |m: &ArgMatches, name: &str| m.get_one::<String>(name).cloned();
    let result = match matches.subcommand() {
        Some(("buy", m)) => match arg(m, "service") {
            Some(service) => bridge.buy(&service).map(|v| vec![v]),
            None => Err(BridgeError::Usage("buy needs a SERVICE".into())),
        },
        Some(("status", m)) => bridge.status(arg(m, "purchase").as_deref()),
        Some(("resume", _)) => bridge.resume(),
        Some(("show", m)) => match arg(m, "purchase") {
            Some(which) => bridge.show(&which).map(|v| vec![v]),
            None => Err(BridgeError::Usage("show needs a PURCHASE".into())),
        },
        _ => Err(BridgeError::Usage("unknown command".into())),
    };
    match result {
        Ok(views) => {
            print_views(&views);
            0
        }
        Err(e) => fail(&e),
    }
}

// The CLI's own output: the notice and errors on stderr, purchases on stdout.
#[allow(clippy::print_stderr)]
fn notice() {
    eprintln!("{SIMULATION_NOTICE}\n");
}

#[allow(clippy::print_stderr)]
fn fail(e: &BridgeError) -> i32 {
    eprintln!("error: {e}");
    e.exit_code()
}

#[allow(clippy::print_stdout)]
fn print_views(views: &[PurchaseView]) {
    if views.is_empty() {
        println!("no open purchases");
    }
    for v in views {
        print_view(v);
    }
}

#[allow(clippy::print_stdout)]
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
