//! Ignore lists + directory/path/extension predicates. Mirrors GitNexus's
//! `src/config/ignore-service.ts` (`DEFAULT_IGNORE_LIST` / `IGNORED_EXTENSIONS`).
//! `.gitignore` itself is honored by the `ignore` crate in `walk.rs`.

const DEFAULT_IGNORE_LIST: &[&str] = &[
    ".git",
    ".svn",
    ".hg",
    ".bzr",
    ".idea",
    ".vscode",
    ".vs",
    ".eclipse",
    ".settings",
    ".ds_store",
    "thumbs.db",
    "node_modules",
    "bower_components",
    "jspm_packages",
    "vendor",
    "third_party",
    "3rdparty",
    "venv",
    ".venv",
    "env",
    ".env",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    "site-packages",
    ".tox",
    "eggs",
    ".eggs",
    "lib64",
    "parts",
    "sdist",
    "wheels",
    "dist",
    "build",
    "out",
    "output",
    "bin",
    "obj",
    "target",
    ".next",
    ".nuxt",
    ".output",
    ".vercel",
    ".netlify",
    ".serverless",
    "_build",
    "public/build",
    ".parcel-cache",
    ".turbo",
    ".svelte-kit",
    "coverage",
    ".nyc_output",
    "htmlcov",
    ".coverage",
    "__tests__",
    "__mocks__",
    ".jest",
    "logs",
    "log",
    "tmp",
    "temp",
    "cache",
    ".cache",
    ".tmp",
    ".temp",
    ".generated",
    "generated",
    "auto-generated",
    "monaco-workers",
    ".terraform",
    ".husky",
    ".github",
    ".circleci",
    ".gitlab",
    "fixtures",
    "snapshots",
    "__snapshots__",
];

const IGNORED_EXTENSIONS: &[&str] = &[
    ".png",
    ".jpg",
    ".jpeg",
    ".gif",
    ".svg",
    ".ico",
    ".webp",
    ".bmp",
    ".tiff",
    ".tif",
    ".psd",
    ".ai",
    ".sketch",
    ".fig",
    ".xd",
    ".zip",
    ".tar",
    ".gz",
    ".rar",
    ".7z",
    ".bz2",
    ".xz",
    ".tgz",
    ".exe",
    ".dll",
    ".so",
    ".dylib",
    ".a",
    ".lib",
    ".o",
    ".obj",
    ".class",
    ".jar",
    ".war",
    ".ear",
    ".pyc",
    ".pyo",
    ".pyd",
    ".beam",
    ".wasm",
    ".node",
    ".pdf",
    ".doc",
    ".docx",
    ".xls",
    ".xlsx",
    ".ppt",
    ".pptx",
    ".odt",
    ".ods",
    ".odp",
    ".mp4",
    ".mp3",
    ".wav",
    ".mov",
    ".avi",
    ".mkv",
    ".flv",
    ".wmv",
    ".ogg",
    ".webm",
    ".flac",
    ".aac",
    ".m4a",
    ".woff",
    ".woff2",
    ".ttf",
    ".eot",
    ".otf",
    ".db",
    ".sqlite",
    ".sqlite3",
    ".mdb",
    ".accdb",
    ".min.js",
    ".min.css",
    ".bundle.js",
    ".chunk.js",
    ".map",
    ".lock",
    ".pem",
    ".key",
    ".crt",
    ".cer",
    ".p12",
    ".pfx",
    ".csv",
    ".tsv",
    ".parquet",
    ".avro",
    ".feather",
    ".npy",
    ".npz",
    ".pkl",
    ".pickle",
    ".h5",
    ".hdf5",
    ".bin",
    ".dat",
    ".data",
    ".raw",
    ".iso",
    ".img",
    ".dmg",
];

const IGNORED_FILES: &[&str] = &[
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "composer.lock",
    "gemfile.lock",
    "poetry.lock",
    "cargo.lock",
    "go.sum",
    ".gitignore",
    ".gitattributes",
    ".npmrc",
    ".yarnrc",
    ".editorconfig",
    ".prettierrc",
    ".prettierignore",
    ".eslintignore",
    ".dockerignore",
    "thumbs.db",
    ".ds_store",
    "license",
    "license.md",
    "license.txt",
    "changelog.md",
    "changelog",
    "contributing.md",
    "code_of_conduct.md",
    "security.md",
    ".env",
    ".env.local",
    ".env.development",
    ".env.production",
    ".env.test",
    ".env.example",
];

pub fn should_ignore_dir(path: &str) -> bool {
    path.replace('\\', "/")
        .to_ascii_lowercase()
        .split('/')
        .any(|part| contains_ignore(DEFAULT_IGNORE_LIST, part))
}

pub fn should_ignore_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let lower = normalized.to_ascii_lowercase();
    if lower.contains("/storage/framework/views/") || lower.starts_with("storage/framework/views/")
    {
        return true;
    }

    let parts: Vec<&str> = lower.split('/').collect();
    if parts
        .iter()
        .any(|part| contains_ignore(DEFAULT_IGNORE_LIST, part))
    {
        return true;
    }

    let file_name = parts.last().copied().unwrap_or_default();
    if contains_ignore(IGNORED_FILES, file_name) {
        return true;
    }

    if let Some(ext) = extension_like(file_name) {
        if contains_ignore(IGNORED_EXTENSIONS, ext) {
            return true;
        }
    }
    if let Some(ext) = compound_extension_like(file_name) {
        if contains_ignore(IGNORED_EXTENSIONS, ext) {
            return true;
        }
    }

    file_name.contains(".bundle.")
        || file_name.contains(".chunk.")
        || file_name.contains(".generated.")
        || file_name.ends_with(".d.ts")
}

fn contains_ignore(list: &[&str], needle: &str) -> bool {
    list.contains(&needle)
}

fn extension_like(file_name: &str) -> Option<&str> {
    file_name.rfind('.').map(|idx| &file_name[idx..])
}

fn compound_extension_like(file_name: &str) -> Option<&str> {
    let last = file_name.rfind('.')?;
    let second = file_name[..last].rfind('.')?;
    Some(&file_name[second..])
}


