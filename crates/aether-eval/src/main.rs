use aether_eval::{compact_capability_summary, compact_summary, run_capability_suite, run_suite};
use std::path::PathBuf;

fn main() {
    let mut arguments = std::env::args_os().skip(1);
    let first = arguments.next();
    let capability = first.as_deref().is_some_and(|argument| argument == "--capability");
    let output_argument = if capability { arguments.next() } else { first };
    let output = output_argument
        .or_else(|| {
            (!capability).then(|| std::ffi::OsString::from("target/aether-eval-results.json"))
        })
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/aether-eval-capability-results.json"));
    if arguments.next().is_some() {
        eprintln!("usage: aether-eval [--capability] [RESULTS.json]");
        std::process::exit(2);
    }
    if capability {
        match run_capability_suite(Some(&output)) {
            Ok(suite) => {
                print!("{}", compact_capability_summary(&suite));
                println!("json: {}", output.display());
                if !suite.success {
                    std::process::exit(1);
                }
            }
            Err(error) => {
                eprintln!("capability evaluation failed: {error}");
                std::process::exit(1);
            }
        }
    } else {
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
}
