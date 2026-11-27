//! `paid-services-sidecar`: a simulation-only CLI. See the repository README.

use std::path::PathBuf;

use clap::{value_parser, Arg, ArgMatches, Command};
use paid_services_sidecar::bridge::{
    load_config, Bridge, BridgeError, PaymentRequest, PurchaseView, SIMULATION_NOTICE,
};
use paid_services_sidecar::state::Stage;
use paid_services_types::codec::Entitlement;
use paid_services_types::uri::format_zec;
use qrcode::render::unicode::Dense1x2;
use qrcode::QrCode;

/// The payment request as a terminal QR code, for a wallet to scan. Simulation only: the
/// mock requests are not meant to be paid.
fn qr(uri: &str) -> Option<String> {
    let code = QrCode::new(uri.as_bytes()).ok()?;
    Some(code.render::<Dense1x2>().quiet_zone(true).build())
}

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
                .about("Start a purchase of an offer, or continue the open one for that offer")
                .arg(
                    Arg::new("kind")
                        .value_name("SERVICE_KIND")
                        .required(true)
                        .help("A versioned service kind, for example zakura.relay.blockstream/1"),
                )
                .arg(Arg::new("offer").value_name("OFFER_ID").required(true)),
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
        Some(("buy", m)) => match (arg(m, "kind"), arg(m, "offer")) {
            (Some(kind), Some(offer)) => bridge.buy(&kind, &offer).map(|v| vec![v]),
            _ => Err(BridgeError::Usage(
                "buy needs a SERVICE_KIND and an OFFER_ID".into(),
            )),
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
        "  service {} offer {}  stage {}  provider status {}",
        v.service_kind,
        v.offer_id,
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
            match qr(uri) {
                Some(code) => println!("{code}"),
                None => println!("    (the request is too long for a QR code)"),
            }
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
    match (v.stage, v.entitlement) {
        (Stage::Redeemed, Some(Entitlement::HeightWindow { start_height, end_height })) => println!(
            "  grant redeemed for blocks [{start_height}, {end_height}) (mock install; no stream is consumed)"
        ),
        (_, Some(e)) => println!("  grant entitlement {e:?}"),
        _ => {}
    }
    if let Some(n) = &v.note {
        println!("  note: {n}");
    }
}
