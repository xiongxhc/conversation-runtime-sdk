#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const HEADER: &str = "sample_id,user_speech_onset_ms,last_response_waveform_ms,user_speech_end_ms,first_response_waveform_ms,valid,exclusion_reason\n";

#[test]
fn reports_nearest_rank_percentiles_for_thirty_valid_samples() {
    let mut csv = HEADER.to_owned();
    for sample in 1..=30_u64 {
        csv.push_str(&valid_row(&sample.to_string(), sample, 100 + sample));
    }

    let output = run_report(&csv);

    assert!(output.status.success(), "{output:?}");
    let report = report(&output);
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["valid_sample_count"], 30);
    assert_eq!(report["excluded_sample_count"], 0);
    assert_eq!(report["audible_stop_latency_ms"]["p50"], 15);
    assert_eq!(report["audible_stop_latency_ms"]["p95"], 29);
    assert_eq!(report["audible_stop_latency_ms"]["maximum"], 30);
    assert_eq!(report["speech_end_to_first_audible_ms"]["p50"], 115);
    assert_eq!(report["speech_end_to_first_audible_ms"]["p95"], 129);
    assert_eq!(report["speech_end_to_first_audible_ms"]["maximum"], 130);
    assert_eq!(report["audible_stop_threshold_ms"], 500);
    assert_eq!(report["pass"], true);
}

#[test]
fn reports_only_enumerated_exclusion_reason_counts() {
    let mut csv = thirty_valid_rows();
    csv.push_str("31,,,,,false,recorder-failure\n");
    csv.push_str("32,,,,,false,annotation-error\n");

    let output = run_report(&csv);

    assert!(output.status.success(), "{output:?}");
    let report = report(&output);
    assert_eq!(report["valid_sample_count"], 30);
    assert_eq!(report["excluded_sample_count"], 2);
    assert_eq!(report["excluded_reason_counts"]["annotation-error"], 1);
    assert_eq!(report["excluded_reason_counts"]["recorder-failure"], 1);
    assert!(report.get("excluded_samples").is_none());
}

#[test]
fn exits_nonzero_with_a_report_when_audible_stop_p95_exceeds_threshold() {
    let mut csv = HEADER.to_owned();
    for sample in 1..=28_u64 {
        csv.push_str(&valid_row(&sample.to_string(), 100, 100));
    }
    csv.push_str(&valid_row("29", 501, 100));
    csv.push_str(&valid_row("30", 501, 100));

    let output = run_report(&csv);

    assert!(!output.status.success(), "{output:?}");
    let report = report(&output);
    assert_eq!(report["audible_stop_latency_ms"]["p95"], 501);
    assert_eq!(report["audible_stop_threshold_ms"], 500);
    assert_eq!(report["pass"], false);
}

#[test]
fn rejects_fewer_than_thirty_valid_samples() {
    let mut csv = HEADER.to_owned();
    for sample in 1..30_u64 {
        csv.push_str(&valid_row(&sample.to_string(), 100, 100));
    }

    assert_rejected(&csv);
}

#[test]
fn rejects_duplicate_sample_identifiers() {
    let mut csv = thirty_valid_rows();
    csv.push_str(&valid_row("1", 100, 100));

    assert_rejected(&csv);
}

#[test]
fn rejects_malformed_values_and_validity_contracts() {
    let cases = [
        format!("{HEADER}1,nope,2,3,4,true,\n"),
        format!("{HEADER}1,1,2,3,4,yes,\n"),
        format!("{HEADER}1,1,2,3,4,true,should-be-empty\n"),
        format!("{HEADER}1,,,,,false,\n"),
        format!("{HEADER}1,,,,,false,private-reason\n"),
        format!("{HEADER}01,1,2,3,4,true,\n"),
        "wrong,header\n1,1,2,3,4,true,\n".to_owned(),
    ];

    for csv in cases {
        assert_rejected(&csv);
    }
}

#[test]
fn rejects_non_monotonic_user_speech_timestamps() {
    let csv = format!("{HEADER}1,20,30,10,40,true,\n");

    assert_rejected(&csv);
}

#[test]
fn rejects_latency_overflow_and_negative_latency() {
    let overflow = format!("{HEADER}1,0,{},1,2,true,\n", u64::MAX);
    assert_rejected(&overflow);

    let mut csv = HEADER.to_owned();
    csv.push_str("1,100,90,200,190,true,\n");
    for sample in 2..=30_u64 {
        csv.push_str(&valid_row(&sample.to_string(), 100, 100));
    }
    assert_rejected(&csv);
}

#[test]
fn failures_do_not_echo_paths_or_unapproved_columns() {
    let private_text = "private transcript content";
    let csv = format!("{HEADER}1,1,2,3,4,true,,{private_text}\n");
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("private-recording-name.csv");
    std::fs::write(&path, csv).unwrap();

    let output = Command::new(report_binary())
        .args(["--input", path.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains(private_text), "{stderr}");
    assert!(!stderr.contains(path.to_str().unwrap()), "{stderr}");
}

#[test]
fn requires_an_absolute_input_path() {
    let output = Command::new(report_binary())
        .args(["--input", "relative.csv"])
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
}

fn thirty_valid_rows() -> String {
    let mut csv = HEADER.to_owned();
    for sample in 1..=30_u64 {
        csv.push_str(&valid_row(&sample.to_string(), 100, 100));
    }
    csv
}

fn valid_row(sample_id: &str, audible_stop_ms: u64, first_audible_ms: u64) -> String {
    let onset = 1_000_u64;
    let speech_end = 2_000_u64;
    format!(
        "{sample_id},{onset},{},{speech_end},{},true,\n",
        onset + audible_stop_ms,
        speech_end + first_audible_ms
    )
}

fn run_report(csv: &str) -> Output {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("samples.csv");
    std::fs::write(&path, csv).unwrap();
    Command::new(report_binary())
        .args(["--input", path.to_str().unwrap()])
        .output()
        .unwrap()
}

fn assert_rejected(csv: &str) {
    let output = run_report(csv);
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
}

fn report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON report: {error}; stdout={}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn report_binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_conversation-acoustic-report"))
}
