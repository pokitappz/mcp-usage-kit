//! Prints what each meter would invoice for the same MCP traffic.

use std::process::ExitCode;

use mcp_overbilling::{Meter, RequestCounter, RequestPricing, Suite, TerminalDelivery, measure};

fn main() -> ExitCode {
    let json = std::env::args().any(|arg| arg == "--json");
    let strict = std::env::args().any(|arg| arg == "--strict");

    let suite = Suite::checked_in();
    let mut meters: Vec<Box<dyn Meter>> = vec![
        Box::new(RequestCounter::new("requests", RequestPricing::Flat)),
        Box::new(RequestCounter::new("requests+name", RequestPricing::Named)),
        Box::new(TerminalDelivery::new("delivery")),
    ];
    let report = measure(&suite, &mut meters);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report.to_json())
                .expect("a report is always serializable")
        );
    } else {
        print!("{}", report.to_table());
    }

    // `--strict` is for CI: this project's own meter disagreeing with the
    // declared ground truth is a regression, not a finding about someone else.
    if strict {
        let ours: Vec<_> = report
            .disagreements()
            .into_iter()
            .filter(|(meter, ..)| meter == "delivery")
            .collect();
        if !ours.is_empty() {
            eprintln!("\nterminal-delivery metering disagreed with ground truth:");
            for (meter, scenario, truth, billed) in ours {
                eprintln!("  {scenario}: expected {truth}, {meter} billed {billed}");
            }
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}
