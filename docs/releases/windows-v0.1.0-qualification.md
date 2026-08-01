# Windows v0.1.0 qualification

This is the tracked release-candidate record. The `dev` to `master` pull-request
run produces the first candidate because GitHub enables manual dispatch only
after the workflow exists on the default branch. Later manual runs produce the
same candidate-specific record with the source commit, workflow run, ZIP
filename, SHA-256, CI runner, signing status, and Windows CI result.

- Candidate source commit: pending
- Workflow run: pending
- Package: pending
- SHA-256: pending
- Signing: pending
- Clean Windows 10 x64 build/result: pending
- Clean Windows 11 x64 build/result: pending

After downloading and checksum-verifying the workflow artifact, disconnect the
VM network and run the complete install, `cih doctor`, Unicode/spaced-path index,
serve/MCP/BM25/graph, concurrent re-index, reinstall, uninstall-preserve, and
`-PurgeData` sequence on fresh Windows 10 and Windows 11 x64 snapshots. Any
failure invalidates the candidate; rebuild it rather than modifying the ZIP.
