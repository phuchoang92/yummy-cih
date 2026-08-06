//! Unified portable CIH command line.

use std::ffi::OsString;
use std::net::TcpListener;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(name = "cih", version, about = "Code Intelligence Hub")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Index a repository into the local embedded graph and generate docs.
    Index {
        /// Repository root (default: current directory).
        repo: Option<PathBuf>,
        /// Ignore stage fingerprints and rebuild all enabled stages.
        #[arg(long)]
        force: bool,
        /// Skip documentation generation.
        #[arg(long)]
        no_wiki: bool,
        /// Print the stage summary as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Serve MCP and the graph browser in the foreground.
    Serve {
        /// Indexed repository to select (default: infer from cwd/registry).
        repo: Option<PathBuf>,
        /// Listen address.
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: String,
        /// Open the graph browser after startup.
        #[arg(long)]
        open: bool,
    },
    /// Diagnose the portable installation and local data.
    Doctor {
        /// Optional repository to validate.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Print machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Offline-capable compatibility commands supplied by cih-engine.
    #[command(external_subcommand)]
    Engine(Vec<OsString>),
}

pub fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing();
    match cli.command {
        Command::Index {
            repo,
            force,
            no_wiki,
            json,
        } => index(repo, force, no_wiki, json),
        Command::Serve { repo, bind, open } => serve(repo, bind, open),
        Command::Doctor { repo, json } => doctor(repo, json),
        Command::Engine(args) => engine(args),
    }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
}

fn portable_paths() -> Result<cih_core::CihPaths> {
    cih_core::CihPaths::discover().ok_or_else(|| {
        anyhow!(
            "cannot determine CIH data directory; set CIH_HOME{}",
            if cfg!(windows) {
                " or LOCALAPPDATA"
            } else {
                " or HOME"
            }
        )
    })
}

fn canonical_repo(repo: Option<PathBuf>) -> Result<PathBuf> {
    let repo = repo.unwrap_or(std::env::current_dir().context("cannot read current directory")?);
    repo.canonicalize()
        .with_context(|| format!("repository does not exist: {}", repo.display()))
}

fn repository_identity(repo: &Path) -> Result<cih_core::RepositoryId> {
    let registry = cih_core::Registry::load_snapshot()
        .map(|snapshot| snapshot.registry)
        .unwrap_or_default();
    let preferred = registry.entries.iter().find_map(|entry| {
        same_path(Path::new(&entry.path), repo)
            .then_some(entry.repository_id.as_ref())
            .flatten()
    });
    cih_core::ensure_repository_id(repo, preferred)
}

fn index(repo: Option<PathBuf>, force: bool, no_wiki: bool, json: bool) -> Result<()> {
    let repo = canonical_repo(repo)?;
    let paths = portable_paths()?;
    std::fs::create_dir_all(paths.graphs()).with_context(|| {
        format!(
            "cannot create embedded graph directory {}",
            paths.graphs().display()
        )
    })?;
    let repository_id = repository_identity(&repo)?;
    let graph_key = format!("repo-{repository_id}");

    // The portable product never downloads a decompiler or model implicitly.
    std::env::set_var("CIH_OFFLINE", "1");
    cih_engine::initialize_runtime()?;
    cih_engine::dispatch(
        cih_engine::cmd::args::Command::Refresh(cih_engine::cmd::args::RefreshArgs {
            repo,
            db: cih_engine::cmd::args::DbArgs {
                backend: None,
                falkor_url: None,
                graph_key: None,
                no_load: false,
            },
            json,
            force,
            no_analyze: false,
            no_discover: false,
            no_wiki,
            wiki_mode: Some("graph".to_string()),
            grouping: Some("package".to_string()),
            wiki_language: Some("en".to_string()),
            wiki_out: None,
            llm: false,
            llm_provider: None,
            llm_api_key_env: None,
            llm_model: None,
            stage_and_swap: true,
        }),
        cih_engine::ProductDefaults {
            backend: Some("ladybug".to_string()),
            store_url: Some(paths.graphs().to_string_lossy().into_owned()),
            graph_key: Some(graph_key),
        },
    )
}

fn engine(args: Vec<OsString>) -> Result<()> {
    let Some(name) = args.first().and_then(|value| value.to_str()) else {
        return Err(anyhow!("missing engine command"));
    };
    if matches!(name, "start" | "embed") {
        return Err(anyhow!(
            "'{name}' is not included in the portable profile; use local Ladybug/BM25 commands"
        ));
    }
    let parsed = cih_engine::cmd::args::Cli::try_parse_from(
        std::iter::once(OsString::from("cih")).chain(args),
    )?;
    if matches!(&parsed.command, cih_engine::cmd::args::Command::Ui) {
        return match cih_engine::cmd::tui::run_tui()? {
            Some(args) => engine(args.into_iter().map(OsString::from).collect()),
            None => Ok(()),
        };
    }
    let paths = portable_paths()?;
    std::fs::create_dir_all(paths.graphs())?;
    std::env::set_var("CIH_OFFLINE", "1");
    cih_engine::initialize_runtime()?;
    cih_engine::dispatch(
        parsed.command,
        cih_engine::ProductDefaults {
            backend: Some("ladybug".to_string()),
            store_url: Some(paths.graphs().to_string_lossy().into_owned()),
            graph_key: None,
        },
    )
}

fn serve(repo: Option<PathBuf>, bind: String, open: bool) -> Result<()> {
    let paths = portable_paths()?;
    let entry = select_primary_repository(repo)?;
    let artifacts_dir = Path::new(&entry.artifacts_dir)
        .parent()
        .map(Path::to_path_buf);
    let current_exe =
        std::env::current_exe().context("cannot locate the running cih executable")?;
    if open {
        schedule_browser_open(format!("http://{bind}/graph"));
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to initialize server runtime")?;
    runtime.block_on(cih_server::run_with_config(cih_server::ServeConfig {
        bind,
        backend: "ladybug".to_string(),
        store_url: paths.graphs().to_string_lossy().into_owned(),
        graph_key: entry.graph_key,
        artifacts_dir,
        index_program: cih_server::IndexProgram {
            program: current_exe,
            prefix_args: Vec::new(),
        },
    }))
}

fn select_primary_repository(explicit: Option<PathBuf>) -> Result<cih_core::RegistryEntry> {
    let snapshot = cih_core::Registry::load_snapshot()
        .context("cannot read the CIH registry; run `cih doctor` to diagnose its integrity")?;
    let valid: Vec<_> = snapshot
        .registry
        .entries
        .into_iter()
        .filter(|entry| Path::new(&entry.path).is_dir())
        .collect();
    if let Some(repo) = explicit {
        let repo = canonical_repo(Some(repo))?;
        return valid
            .into_iter()
            .find(|entry| same_path(Path::new(&entry.path), &repo))
            .ok_or_else(|| {
                anyhow!(
                    "repository '{}' is not indexed; run `cih index {}` first",
                    repo.display(),
                    repo.display()
                )
            });
    }

    let cwd = std::env::current_dir()
        .context("cannot read current directory")?
        .canonicalize()
        .context("cannot canonicalize current directory")?;
    let mut containing: Vec<_> = valid
        .iter()
        .filter_map(|entry| {
            Path::new(&entry.path)
                .canonicalize()
                .ok()
                .filter(|path| cwd.starts_with(path))
                .map(|path| (path.components().count(), entry.clone()))
        })
        .collect();
    containing.sort_by_key(|(depth, _)| std::cmp::Reverse(*depth));
    if let Some((best_depth, best)) = containing.first() {
        if containing
            .get(1)
            .is_none_or(|(depth, _)| depth < best_depth)
        {
            return Ok(best.clone());
        }
    }
    if valid.len() == 1 {
        return Ok(valid.into_iter().next().expect("length checked"));
    }
    if valid.is_empty() {
        return Err(anyhow!(
            "no indexed repositories; run `cih index [REPO]` first"
        ));
    }
    let choices = valid
        .iter()
        .map(|entry| format!("{} ({})", entry.name, entry.path))
        .collect::<Vec<_>>()
        .join(", ");
    Err(anyhow!(
        "multiple indexed repositories are available; pass one to `cih serve REPO`. Available: {choices}"
    ))
}

fn same_path(left: &Path, right: &Path) -> bool {
    left.canonicalize()
        .map(|path| path == right)
        .unwrap_or_else(|_| left == right)
}

fn schedule_browser_open(url: String) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(750));
        #[cfg(windows)]
        let result = std::process::Command::new("cmd")
            .args(["/C", "start", "", &url])
            .spawn();
        #[cfg(target_os = "macos")]
        let result = std::process::Command::new("open").arg(&url).spawn();
        #[cfg(all(unix, not(target_os = "macos")))]
        let result = std::process::Command::new("xdg-open").arg(&url).spawn();
        if let Err(error) = result {
            tracing::warn!(%error, %url, "could not open graph browser");
        }
    });
}

#[derive(Serialize)]
struct DoctorReport {
    ok: bool,
    home: Check,
    registry: Check,
    embedded_graph: Check,
    native_runtime: Check,
    port: Check,
    repository: Check,
    legacy_windows_home: Check,
}

#[derive(Serialize)]
struct Check {
    ok: bool,
    message: String,
}

impl Check {
    fn pass(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
        }
    }

    fn fail(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: message.into(),
        }
    }
}

fn doctor(repo: Option<PathBuf>, json: bool) -> Result<()> {
    let paths = portable_paths()?;
    let home = check_home(&paths);
    let registry = match cih_core::Registry::load_snapshot() {
        Ok(snapshot) => Check::pass(format!(
            "{} valid entries (revision {})",
            snapshot.registry.entries.len(),
            snapshot.revision.sequence
        )),
        Err(error) => Check::fail(error.to_string()),
    };
    let embedded_graph = check_embedded_graph(&paths);
    let native_runtime = check_native_runtime();
    let port = match TcpListener::bind("127.0.0.1:8080") {
        Ok(listener) => {
            drop(listener);
            Check::pass("127.0.0.1:8080 is available")
        }
        Err(error) => Check::fail(format!("127.0.0.1:8080 unavailable: {error}")),
    };
    let repository = match repo {
        Some(repo) => match canonical_repo(Some(repo)) {
            Ok(repo) => match cih_core::load_repository_id(&repo) {
                Ok(Some(id)) => Check::pass(format!("{} ({id})", repo.display())),
                Ok(None) => Check::fail(format!(
                    "{} has no CIH identity; run `cih index`",
                    repo.display()
                )),
                Err(error) => Check::fail(error.to_string()),
            },
            Err(error) => Check::fail(error.to_string()),
        },
        None => Check::pass("not requested"),
    };
    let legacy_windows_home = check_legacy_windows_home(&paths);
    let ok = [
        &home,
        &registry,
        &embedded_graph,
        &native_runtime,
        &port,
        &repository,
    ]
    .iter()
    .all(|check| check.ok);
    let report = DoctorReport {
        ok,
        home,
        registry,
        embedded_graph,
        native_runtime,
        port,
        repository,
        legacy_windows_home,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_doctor(&report);
    }
    if report.ok {
        Ok(())
    } else {
        Err(anyhow!("one or more doctor checks failed"))
    }
}

fn check_home(paths: &cih_core::CihPaths) -> Check {
    let result = (|| -> Result<()> {
        std::fs::create_dir_all(paths.graphs())?;
        let probe = paths
            .home()
            .join(format!(".write-probe-{}", std::process::id()));
        std::fs::write(&probe, b"cih")?;
        std::fs::remove_file(probe)?;
        Ok(())
    })();
    match result {
        Ok(()) => Check::pass(format!("{} is writable", paths.home().display())),
        Err(error) => Check::fail(format!("{}: {error}", paths.home().display())),
    }
}

fn check_embedded_graph(paths: &cih_core::CihPaths) -> Check {
    let key = format!("doctor-{}", std::process::id());
    let result = (|| -> Result<()> {
        let store = cih_store_factory::connect_store(
            "ladybug",
            &paths.graphs().to_string_lossy(),
            &key,
            &cih_store_factory::StoreOptions::default(),
        )?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            store.backend_readiness().await?;
            store.drop_graph().await
        })?;
        Ok(())
    })();
    match result {
        Ok(()) => Check::pass("Ladybug open/readiness/drop succeeded"),
        Err(error) => Check::fail(error.to_string()),
    }
}

#[cfg(windows)]
fn check_native_runtime() -> Check {
    let directory = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    let Some(directory) = directory else {
        return Check::fail("cannot locate cih.exe directory");
    };
    let names = std::fs::read_dir(&directory)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .map(|name| name.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let missing = ["libssl-3*.dll", "libcrypto-3*.dll"]
        .into_iter()
        .filter(|pattern| {
            let prefix = pattern.trim_end_matches("*.dll");
            !names
                .iter()
                .any(|name| name.starts_with(prefix) && name.ends_with(".dll"))
        })
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Check::pass("required OpenSSL DLLs are present")
    } else {
        Check::fail(format!("missing companion DLLs: {}", missing.join(", ")))
    }
}

#[cfg(target_os = "linux")]
fn check_native_runtime() -> Check {
    let maps = match std::fs::read_to_string("/proc/self/maps") {
        Ok(maps) => maps,
        Err(error) => {
            return Check::fail(format!("cannot inspect loaded Linux libraries: {error}"));
        }
    };
    check_linux_native_runtime_maps(&maps)
}

#[cfg(target_os = "linux")]
fn check_linux_native_runtime_maps(maps: &str) -> Check {
    let required = ["liblbug.so", "libssl.so.3", "libcrypto.so.3"];
    let missing = required
        .into_iter()
        .filter(|name| !maps.lines().any(|line| line.contains(name)))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Check::pass("Ladybug and OpenSSL 3 shared libraries are loaded")
    } else {
        Check::fail(format!(
            "required Linux shared libraries are not loaded: {}",
            missing.join(", ")
        ))
    }
}

#[cfg(all(not(windows), not(target_os = "linux")))]
fn check_native_runtime() -> Check {
    Check::pass("native-runtime bundle check not applicable")
}

#[cfg(windows)]
fn check_legacy_windows_home(paths: &cih_core::CihPaths) -> Check {
    match cih_core::CihPaths::legacy_windows_home() {
        Some(legacy) if legacy != paths.home() && legacy.exists() => Check::pass(format!(
            "legacy data found at {}; copy it to {} or set CIH_HOME={} to keep using it",
            legacy.display(),
            paths.home().display(),
            legacy.display()
        )),
        _ => Check::pass("no legacy %USERPROFILE%\\.cih data detected"),
    }
}

#[cfg(not(windows))]
fn check_legacy_windows_home(_paths: &cih_core::CihPaths) -> Check {
    Check::pass("not applicable")
}

fn print_doctor(report: &DoctorReport) {
    println!("CIH doctor: {}", if report.ok { "ok" } else { "failed" });
    for (name, check) in [
        ("home", &report.home),
        ("registry", &report.registry),
        ("embedded graph", &report.embedded_graph),
        ("native runtime", &report.native_runtime),
        ("port", &report.port),
        ("repository", &report.repository),
        ("legacy Windows home", &report.legacy_windows_home),
    ] {
        println!(
            "  {:<20} {}  {}",
            name,
            if check.ok { "ok" } else { "FAIL" },
            check.message
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_engine_surface_rejects_portable_exclusions() {
        assert!(engine(vec![OsString::from("start")])
            .unwrap_err()
            .to_string()
            .contains("not included"));
        assert!(engine(vec![OsString::from("embed")])
            .unwrap_err()
            .to_string()
            .contains("not included"));
    }

    #[test]
    fn cli_defaults_repo_arguments() {
        let cli = Cli::try_parse_from(["cih", "index", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Index {
                repo: None,
                json: true,
                ..
            }
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_native_runtime_check_reports_missing_companions() {
        let complete = check_linux_native_runtime_maps(
            "/bundle/lib/liblbug.so.0.18.2\n/bundle/lib/libssl.so.3\n/bundle/lib/libcrypto.so.3",
        );
        assert!(complete.ok);

        let incomplete = check_linux_native_runtime_maps("/bundle/lib/liblbug.so.0.18.2");
        assert!(!incomplete.ok);
        assert!(incomplete.message.contains("libssl.so.3"));
        assert!(incomplete.message.contains("libcrypto.so.3"));
    }
}
