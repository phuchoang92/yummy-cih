//! Git CLI adapter for the changed-files source port.

use crate::ports::changed_files_source::{ChangeScope, ChangedFilesSource};

#[derive(Default)]
pub(crate) struct GitChangedFilesSource;

impl ChangedFilesSource for GitChangedFilesSource {
    fn changed_files(
        &self,
        repo_path: &str,
        scope: ChangeScope,
        base_ref: Option<&str>,
    ) -> Result<Vec<String>, String> {
        let mut command = std::process::Command::new("git");
        command.arg("diff").arg("--name-only");
        match scope {
            ChangeScope::Staged => {
                command.arg("--cached").arg("HEAD");
            }
            ChangeScope::BaseRef => {
                let base_ref = base_ref.ok_or_else(|| {
                    "`base_ref` scope requires the `base_ref` argument".to_string()
                })?;
                command.arg(base_ref);
            }
            ChangeScope::Working => {
                command.arg("HEAD");
            }
        }
        command.arg("--").current_dir(repo_path);
        let output = command
            .output()
            .map_err(|error| format!("git diff failed: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "git diff error: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(String::from)
            .collect())
    }
}
