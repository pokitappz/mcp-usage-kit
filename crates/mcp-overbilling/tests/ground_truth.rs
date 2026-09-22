//! The suite has to be evidence, which means it has to be able to fail.
//!
//! Two things are checked here. That this project's meter agrees with every
//! declared ground truth, so the comparison is a claim we are also held to.
//! And that the scenario set is not arranged to flatter the answer.

use mcp_overbilling::{
    Category, Meter, RequestCounter, RequestPricing, Suite, TerminalDelivery, measure,
};

fn meters() -> Vec<Box<dyn Meter>> {
    vec![
        Box::new(RequestCounter::new("requests", RequestPricing::Flat)),
        Box::new(RequestCounter::new("requests+name", RequestPricing::Named)),
        Box::new(TerminalDelivery::new("delivery")),
    ]
}

#[test]
fn terminal_delivery_matches_every_declared_ground_truth() {
    // The whole comparison rests on `groundTruthUnits` being the right answer
    // rather than a description of what our engine happens to do. Publishing a
    // number our own meter cannot reproduce would be the worst version of this
    // suite, so it fails here rather than in someone else's review of it.
    let suite = Suite::checked_in();
    let mut meters = meters();
    let report = measure(&suite, &mut meters);

    let ours: Vec<_> = report
        .disagreements()
        .into_iter()
        .filter(|(meter, ..)| meter == "delivery")
        .collect();

    assert!(
        ours.is_empty(),
        "terminal-delivery metering disagreed with ground truth: {ours:?}"
    );
}

#[test]
fn every_category_the_report_prints_has_scenarios_behind_it() {
    let suite = Suite::checked_in();
    let mut meters = meters();
    let report = measure(&suite, &mut meters);

    for category in Category::ALL {
        let totals = report
            .category(category)
            .unwrap_or_else(|| panic!("{} has no scenarios", category.as_str()));
        assert!(
            totals.scenarios > 0,
            "{} is printed but empty",
            category.as_str()
        );
    }
}

#[test]
fn the_suite_contains_traffic_a_request_counter_gets_right() {
    // A comparison built only from traffic that favours one answer is
    // marketing, not measurement. At least one scenario has to be a case where
    // counting requests lands on the correct invoice, or the totals are
    // constructed rather than observed.
    let suite = Suite::checked_in();
    let mut meters = meters();
    let report = measure(&suite, &mut meters);

    let agreements = report
        .outcomes
        .iter()
        .filter(|outcome| outcome.billed.get("requests+name") == Some(&outcome.ground_truth))
        .count();

    assert!(
        agreements >= 2,
        "the suite needs scenarios where request counting is correct; found {agreements}"
    );
}

#[test]
fn every_scenario_states_why_its_ground_truth_is_the_right_number() {
    // The rationale is the part a skeptic argues with. A scenario without one
    // is an assertion, and an assertion is not evidence.
    for scenario in Suite::checked_in().scenarios {
        assert!(
            scenario.ground_truth_rationale.len() > 80,
            "{} needs a real rationale, not a label",
            scenario.id
        );
        assert!(
            !scenario.exchanges.is_empty(),
            "{} has no traffic",
            scenario.id
        );
    }
}

#[test]
fn the_report_serializes_to_stable_machine_readable_json() {
    // The suite is meant to be run by other people against other meters, so
    // the JSON is an interface, not debug output.
    let suite = Suite::checked_in();
    let mut meters = meters();
    let json = measure(&suite, &mut meters).to_json();

    assert!(json["totals"]["groundTruthUnits"].as_u64().unwrap() > 0);
    assert_eq!(
        json["scenarios"].as_array().unwrap().len(),
        suite.scenarios.len()
    );
    for category in json["categories"].as_array().unwrap() {
        assert!(category["category"].is_string());
        assert!(category["billedUnits"]["delivery"].is_u64());
    }
}
