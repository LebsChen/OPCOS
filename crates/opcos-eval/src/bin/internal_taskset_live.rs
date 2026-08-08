use opcos_eval::{LiveRolloutConfig, aggregate_taskset_runs, run_live_internal_taskset};

#[tokio::main]
async fn main() {
    let config = match LiveRolloutConfig::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    let runs = match run_live_internal_taskset(&config).await {
        Ok(runs) => runs,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    let aggregate = aggregate_taskset_runs(&runs);
    match serde_json::to_string_pretty(&aggregate) {
        Ok(output) => println!("{output}"),
        Err(error) => {
            eprintln!("failed to serialize rollout results: {error}");
            std::process::exit(1);
        }
    }
}
