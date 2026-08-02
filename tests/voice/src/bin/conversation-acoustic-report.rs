use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::PathBuf;

use serde::Serialize;

const HEADER: &str = "sample_id,user_speech_onset_ms,last_response_waveform_ms,user_speech_end_ms,first_response_waveform_ms,valid,exclusion_reason";
const MAX_INPUT_BYTES: u64 = 1_048_576;
const MAX_SAMPLE_COUNT: usize = 10_000;
const MINIMUM_VALID_SAMPLES: usize = 30;
const AUDIBLE_STOP_THRESHOLD_MS: i64 = 500;

fn main() {
    let exit_code = match run() {
        Ok(pass) => {
            if pass {
                0
            } else {
                1
            }
        }
        Err(error) => {
            eprintln!("acoustic report: {}", error.message());
            1
        }
    };
    std::process::exit(exit_code);
}

fn run() -> Result<bool, ReportError> {
    let input = parse_arguments(std::env::args().skip(1))?;
    let metadata = std::fs::metadata(&input).map_err(|_| ReportError::Input)?;
    if !metadata.is_file() || metadata.len() > MAX_INPUT_BYTES {
        return Err(ReportError::Input);
    }
    let input = std::fs::read_to_string(input).map_err(|_| ReportError::Input)?;
    let samples = parse_samples(&input)?;
    if samples.valid_stop_latencies.len() < MINIMUM_VALID_SAMPLES {
        return Err(ReportError::InsufficientSamples);
    }

    let audible_stop_latency_ms = Distribution::from_values(samples.valid_stop_latencies);
    let speech_end_to_first_audible_ms =
        Distribution::from_values(samples.valid_first_audible_latencies);
    let pass = audible_stop_latency_ms.p95 <= AUDIBLE_STOP_THRESHOLD_MS;
    let report = Report {
        schema_version: 1,
        valid_sample_count: samples.valid_count,
        excluded_sample_count: samples.excluded_count,
        excluded_reason_counts: samples.excluded_reason_counts,
        audible_stop_latency_ms,
        speech_end_to_first_audible_ms,
        audible_stop_threshold_ms: AUDIBLE_STOP_THRESHOLD_MS,
        pass,
    };
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, &report).map_err(|_| ReportError::Output)?;
    output.write_all(b"\n").map_err(|_| ReportError::Output)?;
    Ok(pass)
}

fn parse_arguments(arguments: impl IntoIterator<Item = String>) -> Result<PathBuf, ReportError> {
    let mut arguments = arguments.into_iter();
    let mut input = None;
    while let Some(argument) = arguments.next() {
        if argument != "--input" || input.is_some() {
            return Err(ReportError::Arguments);
        }
        let path = arguments.next().ok_or(ReportError::Arguments)?;
        input = Some(PathBuf::from(path));
    }
    let input = input.ok_or(ReportError::Arguments)?;
    if !input.is_absolute() {
        return Err(ReportError::Arguments);
    }
    Ok(input)
}

fn parse_samples(input: &str) -> Result<Samples, ReportError> {
    let mut lines = input.lines();
    if lines.next() != Some(HEADER) {
        return Err(ReportError::Csv);
    }

    let mut identifiers = HashSet::new();
    let mut samples = Samples::default();
    for line in lines {
        if line.is_empty() || samples.total_count() >= MAX_SAMPLE_COUNT {
            return Err(ReportError::Csv);
        }
        let fields = line.split(',').collect::<Vec<_>>();
        let [sample_id, onset, last_response, speech_end, first_response, valid, reason] =
            fields.as_slice()
        else {
            return Err(ReportError::Csv);
        };
        let sample_id = parse_sample_id(sample_id)?;
        if !identifiers.insert(sample_id) {
            return Err(ReportError::Csv);
        }

        match *valid {
            "true" => {
                if !reason.is_empty() {
                    return Err(ReportError::Csv);
                }
                let onset = parse_timestamp(onset)?;
                let last_response = parse_timestamp(last_response)?;
                let speech_end = parse_timestamp(speech_end)?;
                let first_response = parse_timestamp(first_response)?;
                if onset > speech_end {
                    return Err(ReportError::Csv);
                }
                samples
                    .valid_stop_latencies
                    .push(checked_latency(last_response, onset)?);
                samples
                    .valid_first_audible_latencies
                    .push(checked_latency(first_response, speech_end)?);
                samples.valid_count += 1;
            }
            "false" => {
                let reason = parse_exclusion_reason(reason)?;
                for timestamp in [onset, last_response, speech_end, first_response] {
                    if !timestamp.is_empty() {
                        parse_timestamp(timestamp)?;
                    }
                }
                samples.excluded_count += 1;
                *samples
                    .excluded_reason_counts
                    .entry(reason.to_owned())
                    .or_insert(0) += 1;
            }
            _ => return Err(ReportError::Csv),
        }
    }
    Ok(samples)
}

fn parse_sample_id(value: &str) -> Result<u32, ReportError> {
    let identifier = value.parse::<u32>().map_err(|_| ReportError::Csv)?;
    if identifier == 0 || identifier > MAX_SAMPLE_COUNT as u32 || identifier.to_string() != value {
        return Err(ReportError::Csv);
    }
    Ok(identifier)
}

fn parse_exclusion_reason(value: &str) -> Result<&'static str, ReportError> {
    match value {
        "annotation-error" => Ok("annotation-error"),
        "calibration-error" => Ok("calibration-error"),
        "environment-noise" => Ok("environment-noise"),
        "overlapping-speaker" => Ok("overlapping-speaker"),
        "protocol-deviation" => Ok("protocol-deviation"),
        "recorder-failure" => Ok("recorder-failure"),
        _ => Err(ReportError::Csv),
    }
}

fn parse_timestamp(value: &str) -> Result<u64, ReportError> {
    value.parse().map_err(|_| ReportError::Csv)
}

fn checked_latency(end: u64, start: u64) -> Result<i64, ReportError> {
    let latency = i128::from(end) - i128::from(start);
    if latency < 0 {
        return Err(ReportError::Csv);
    }
    i64::try_from(latency).map_err(|_| ReportError::Csv)
}

#[derive(Default)]
struct Samples {
    valid_count: usize,
    valid_stop_latencies: Vec<i64>,
    valid_first_audible_latencies: Vec<i64>,
    excluded_count: usize,
    excluded_reason_counts: BTreeMap<String, usize>,
}

impl Samples {
    fn total_count(&self) -> usize {
        self.valid_count + self.excluded_count
    }
}

#[derive(Serialize)]
struct Report {
    schema_version: u8,
    valid_sample_count: usize,
    excluded_sample_count: usize,
    excluded_reason_counts: BTreeMap<String, usize>,
    audible_stop_latency_ms: Distribution,
    speech_end_to_first_audible_ms: Distribution,
    audible_stop_threshold_ms: i64,
    pass: bool,
}

#[derive(Serialize)]
struct Distribution {
    p50: i64,
    p95: i64,
    maximum: i64,
}

impl Distribution {
    fn from_values(mut values: Vec<i64>) -> Self {
        values.sort_unstable();
        Self {
            p50: nearest_rank(&values, 50),
            p95: nearest_rank(&values, 95),
            maximum: *values.last().expect("validated sample count is non-zero"),
        }
    }
}

fn nearest_rank(values: &[i64], percentile: usize) -> i64 {
    let rank = (percentile * values.len()).div_ceil(100);
    values[rank - 1]
}

enum ReportError {
    Arguments,
    Input,
    Csv,
    InsufficientSamples,
    Output,
}

impl ReportError {
    const fn message(&self) -> &'static str {
        match self {
            Self::Arguments => "invalid arguments",
            Self::Input => "input is unavailable or exceeds the size limit",
            Self::Csv => "invalid acoustic CSV",
            Self::InsufficientSamples => "at least 30 valid samples are required",
            Self::Output => "could not write report",
        }
    }
}
