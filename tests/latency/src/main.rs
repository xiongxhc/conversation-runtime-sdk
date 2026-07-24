use std::env;
use std::error::Error;

use conversation_latency_harness::measure_mock_turn;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let transcript = env::args()
        .nth(1)
        .unwrap_or_else(|| "hello conversation runtime".into());
    let samples = measure_mock_turn(&transcript).await?;

    println!("event,elapsed_microseconds");
    for sample in samples {
        println!("{},{}", sample.label(), sample.elapsed().as_micros());
    }

    Ok(())
}
