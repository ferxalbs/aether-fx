use aether_eval::{compact_summary, run_suite};
use std::path::PathBuf;

fn main() {
    let mut arguments = std::env::args_os().skip(1);
    let output = arguments
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/aether-eval-results.json"));
    if arguments.next().is_some() {
        eprintln!("usage: aether-eval [RESULTS.json]");
        std::process::exit(2);
    }
    match run_suite(Some(&output)) {
        Ok(suite) => {
            print!("{}", compact_summary(&suite));
            println!("json: {}", output.display());
            if !suite.success {
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!("evaluation failed: {error}");
            std::process::exit(1);
        }
    }
}
