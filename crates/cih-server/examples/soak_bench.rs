//! Scheduled multi-repository soak runner built on the production scale paths.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use cih_server::scale_bench::{run, ScaleConfig};
use serde::Serialize;

#[derive(Debug, Serialize)]
struct SoakReport {
    duration_secs: u64,
    cycles: usize,
    repositories: usize,
    large_repo_nodes: usize,
    small_repo_nodes: usize,
    peak_rss_bytes: Option<u64>,
    acceptance_failures: Vec<String>,
    monotonic_growth_cycles: usize,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let duration_secs = env_u64("CIH_SOAK_DURATION_SECS", 30 * 60)?;
    let repositories = env_usize("CIH_SOAK_REPOSITORIES", 10)?.max(1);
    let large_repo_nodes = env_usize("CIH_SOAK_LARGE_NODES", 500_000)?;
    let small_repo_nodes = env_usize("CIH_SOAK_SMALL_NODES", 50_000)?;
    let iterations = env_usize("CIH_SOAK_ITERATIONS", 10)?.max(1);
    let base = PathBuf::from(
        std::env::var("CIH_SOAK_FIXTURE_DIR").unwrap_or_else(|_| "target/cih-soak-fixtures".into()),
    );
    let output = PathBuf::from(
        std::env::var("CIH_SOAK_OUTPUT").unwrap_or_else(|_| "target/cih-soak-report.json".into()),
    );

    let started = Instant::now();
    let deadline = started + Duration::from_secs(duration_secs);
    let mut cycles = 0_usize;
    let mut peak_rss = None;
    let mut prior_peak = None;
    let mut monotonic_growth_cycles = 0_usize;
    let mut acceptance_failures = Vec::new();

    // Always execute one complete matrix, even for short local smoke runs.
    loop {
        for repository in 0..repositories {
            let nodes = if repository == 0 {
                large_repo_nodes
            } else {
                small_repo_nodes
            };
            let report = run(ScaleConfig {
                fixture_dir: base.join(format!("repo-{repository}")),
                nodes,
                edges_per_node: 2,
                iterations,
                burst_callers: 8,
                search_cache_bytes: 1,
                regenerate: false,
            })
            .await
            .with_context(|| format!("soak repository {repository}"))?;

            for failed in report.acceptance.iter().filter(|result| !result.passed) {
                acceptance_failures.push(format!(
                    "repo-{repository}/{}: {}",
                    failed.name, failed.observed
                ));
            }
            if let Some(observed) = report.memory.observed_peak_rss_bytes {
                peak_rss = Some(peak_rss.map_or(observed, |peak: u64| peak.max(observed)));
                if prior_peak.is_some_and(|prior| observed > prior + prior / 20) {
                    monotonic_growth_cycles += 1;
                } else {
                    monotonic_growth_cycles = 0;
                }
                prior_peak = Some(observed);
            }
        }
        cycles += 1;
        if Instant::now() >= deadline {
            break;
        }
    }

    let report = SoakReport {
        duration_secs: started.elapsed().as_secs(),
        cycles,
        repositories,
        large_repo_nodes,
        small_repo_nodes,
        peak_rss_bytes: peak_rss,
        acceptance_failures,
        monotonic_growth_cycles,
    };
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        &output,
        format!("{}\n", serde_json::to_string_pretty(&report)?),
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);

    if !report.acceptance_failures.is_empty() {
        bail!(
            "{} performance acceptance checks failed",
            report.acceptance_failures.len()
        );
    }
    if report.monotonic_growth_cycles >= 5 {
        bail!("RSS grew by more than 5% for five consecutive repository runs");
    }
    Ok(())
}

fn env_usize(name: &str, default: usize) -> Result<usize> {
    std::env::var(name)
        .ok()
        .map(|value| value.parse().with_context(|| format!("invalid {name}")))
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn env_u64(name: &str, default: u64) -> Result<u64> {
    std::env::var(name)
        .ok()
        .map(|value| value.parse().with_context(|| format!("invalid {name}")))
        .transpose()
        .map(|value| value.unwrap_or(default))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_environment_uses_defaults() {
        assert_eq!(env_usize("CIH_TEST_MISSING_USIZE", 7).unwrap(), 7);
        assert_eq!(env_u64("CIH_TEST_MISSING_U64", 9).unwrap(), 9);
    }
}
