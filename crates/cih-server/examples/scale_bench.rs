//! Generate a deterministic large graph fixture and benchmark server read paths.
//!
//! Recommended reference run:
//!
//! cargo run --release -p cih-server --example scale_bench -- \
//!   --nodes 500000 --edges-per-node 2 --iterations 20 \
//!   --output docs/perf/scale-500k-local.json --enforce

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use cih_server::scale_bench::{run, ScaleConfig};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args = Args::parse(std::env::args().skip(1))?;
    if cfg!(debug_assertions) {
        eprintln!("warning: scale benchmarks should be run with --release");
    }
    let report = run(ScaleConfig {
        fixture_dir: args.fixture_dir,
        nodes: args.nodes,
        edges_per_node: args.edges_per_node,
        iterations: args.iterations,
        burst_callers: args.burst_callers,
        search_cache_bytes: args.search_cache_bytes,
        regenerate: args.regenerate,
    })
    .await?;
    let json = serde_json::to_string_pretty(&report)?;
    println!("{json}");
    if let Some(output) = args.output {
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(&output, format!("{json}\n"))
            .with_context(|| format!("write {}", output.display()))?;
        eprintln!("wrote {}", output.display());
    }
    if args.enforce {
        let failures = report
            .acceptance
            .iter()
            .filter(|result| !result.passed)
            .map(|result| result.name)
            .collect::<Vec<_>>();
        if !failures.is_empty() {
            bail!("performance acceptance failed: {}", failures.join(", "));
        }
    }
    Ok(())
}

struct Args {
    fixture_dir: PathBuf,
    output: Option<PathBuf>,
    nodes: usize,
    edges_per_node: usize,
    iterations: usize,
    burst_callers: usize,
    search_cache_bytes: usize,
    regenerate: bool,
    enforce: bool,
}

impl Args {
    fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut args = Self {
            fixture_dir: PathBuf::from("target/cih-scale-fixtures/500k-1m"),
            output: None,
            nodes: 500_000,
            edges_per_node: 2,
            iterations: 20,
            burst_callers: 16,
            search_cache_bytes: 1,
            regenerate: false,
            enforce: false,
        };
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--fixture-dir" => {
                    args.fixture_dir = PathBuf::from(required_value(&mut arguments, &argument)?);
                }
                "--output" => {
                    args.output = Some(PathBuf::from(required_value(&mut arguments, &argument)?));
                }
                "--nodes" => {
                    args.nodes =
                        parse_usize(required_value(&mut arguments, &argument)?, &argument)?;
                }
                "--edges-per-node" => {
                    args.edges_per_node =
                        parse_usize(required_value(&mut arguments, &argument)?, &argument)?;
                }
                "--iterations" => {
                    args.iterations =
                        parse_usize(required_value(&mut arguments, &argument)?, &argument)?;
                }
                "--burst-callers" => {
                    args.burst_callers =
                        parse_usize(required_value(&mut arguments, &argument)?, &argument)?;
                }
                "--search-cache-bytes" => {
                    args.search_cache_bytes =
                        parse_usize(required_value(&mut arguments, &argument)?, &argument)?;
                }
                "--regenerate" => args.regenerate = true,
                "--enforce" => args.enforce = true,
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                other => bail!("unknown argument '{other}'; use --help"),
            }
        }
        Ok(args)
    }
}

fn required_value(arguments: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    arguments
        .next()
        .with_context(|| format!("{flag} requires a value"))
}

fn parse_usize(value: String, flag: &str) -> Result<usize> {
    value
        .parse()
        .with_context(|| format!("{flag} requires a positive integer, got '{value}'"))
}

fn print_help() {
    println!(
        "cih-server scale benchmark\n\
         \n\
         Options:\n\
           --fixture-dir PATH     Generated/reused fixture directory\n\
           --output PATH          Also write the JSON report to this path\n\
           --nodes N              Node count (default 500000)\n\
           --edges-per-node N     Edges per node (default 2 = 1m edges)\n\
           --iterations N         Warm measurement samples (default 20)\n\
           --burst-callers N      Concurrent same-key cold callers (default 16)\n\
           --search-cache-bytes N Search burst cache budget (default 1 = oversize mode)\n\
           --regenerate           Replace an otherwise reusable fixture\n\
           --enforce              Exit non-zero when an acceptance check fails\n\
           -h, --help             Show this help"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_overrides_and_flags() {
        let args = Args::parse(
            [
                "--nodes",
                "1000",
                "--edges-per-node",
                "3",
                "--iterations",
                "4",
                "--burst-callers",
                "2",
                "--fixture-dir",
                "/tmp/scale",
                "--search-cache-bytes",
                "4096",
                "--regenerate",
                "--enforce",
            ]
            .into_iter()
            .map(String::from),
        )
        .unwrap();
        assert_eq!(args.nodes, 1_000);
        assert_eq!(args.edges_per_node, 3);
        assert_eq!(args.iterations, 4);
        assert_eq!(args.burst_callers, 2);
        assert_eq!(args.search_cache_bytes, 4096);
        assert!(args.regenerate);
        assert!(args.enforce);
    }
}
