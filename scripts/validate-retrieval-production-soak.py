#!/usr/bin/env python3
"""Run the real eight-repository, 30-minute CIH retrieval acceptance gate.

The runner must execute on Linux in the same PID namespace as ``cih-server``.
It records timings, counters, artifact identities, and process-memory statistics;
it never writes repository selectors, paths, queries, patterns, or tool bodies to
the report.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import datetime as dt
import hashlib
import importlib.util
import json
import math
import os
import statistics
import struct
import sys
import threading
import time
import urllib.parse
from dataclasses import dataclass
from pathlib import Path
from typing import Any


MIB = 1024 * 1024
REQUIRED_REPOSITORIES = 8
MINIMUM_DURATION_SECS = 30 * 60
DEFAULT_WARMUP_SECS = 5 * 60
DEFAULT_CACHE_MAX_ENTRIES = 32
DEFAULT_CACHE_MAX_BYTES = 256 * MIB
COUNTER_CACHE_FIELDS = (
    "requests",
    "hits",
    "misses",
    "builds",
    "evictions",
    "oversize",
)
COUNTER_RUNTIME_FIELDS = (
    "scorer_rejected",
    "score_completed",
    "cold_rejected",
    "cold_completed",
    "flight_joined",
    "sidecar_loaded",
    "sidecar_missing",
    "sidecar_stale",
    "sidecar_corrupt",
    "fallback_builds",
    "repair_succeeded",
    "repair_failed",
)
MIXED_TOOLS = ("search_code", "architecture_overview", "grep_files")
BLAKE3_IV = (
    0x6A09E667,
    0xBB67AE85,
    0x3C6EF372,
    0xA54FF53A,
    0x510E527F,
    0x9B05688C,
    0x1F83D9AB,
    0x5BE0CD19,
)
BLAKE3_MESSAGE_PERMUTATION = (2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8)
BLAKE3_CHUNK_START = 1
BLAKE3_CHUNK_END = 2
BLAKE3_ROOT = 8


def load_production_support() -> Any:
    path = Path(__file__).with_name("validate-retrieval-production.py")
    spec = importlib.util.spec_from_file_location("cih_retrieval_validation", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load production validation support from {path}")
    module = importlib.util.module_from_spec(spec)
    # dataclasses resolves annotations through sys.modules while the module loads.
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


SUPPORT = load_production_support()
JsonHttpClient = SUPPORT.JsonHttpClient
McpClient = SUPPORT.McpClient
ValidationError = SUPPORT.ValidationError
add_gate = SUPPORT.add_gate
canonical_result = SUPPORT.canonical_result
integer = SUPPORT.integer
nested = SUPPORT.nested
percentile = SUPPORT.percentile
search_cache = SUPPORT.search_cache
search_runtime = SUPPORT.search_runtime
write_report = SUPPORT.write_report


@dataclass(frozen=True)
class RepositorySpec:
    label_hash: str
    selector: str
    artifacts_root: Path
    artifact_version: str
    expected_repository_id: str
    nodes_sha256: str
    nodes_bytes: int
    edges_bytes: int
    sidecar_bytes: int
    search_query: str
    grep_pattern: str
    grep_glob: str


@dataclass(frozen=True)
class ServerProcess:
    pid: int
    start_time_ticks: int
    cache_max_entries: int
    cache_max_bytes: int
    semantic_enabled: bool
    listener_port: int


def text_hash(value: str, length: int = 16) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()[:length]


def rotate_right_32(value: int, bits: int) -> int:
    return ((value >> bits) | (value << (32 - bits))) & 0xFFFFFFFF


def blake3_mix(
    state: list[int], a: int, b: int, c: int, d: int, left: int, right: int
) -> None:
    state[a] = (state[a] + state[b] + left) & 0xFFFFFFFF
    state[d] = rotate_right_32(state[d] ^ state[a], 16)
    state[c] = (state[c] + state[d]) & 0xFFFFFFFF
    state[b] = rotate_right_32(state[b] ^ state[c], 12)
    state[a] = (state[a] + state[b] + right) & 0xFFFFFFFF
    state[d] = rotate_right_32(state[d] ^ state[a], 8)
    state[c] = (state[c] + state[d]) & 0xFFFFFFFF
    state[b] = rotate_right_32(state[b] ^ state[c], 7)


def blake3_compress(
    chaining_value: tuple[int, ...] | list[int],
    block_words: list[int],
    counter: int,
    block_length: int,
    flags: int,
) -> list[int]:
    state = (
        list(chaining_value)
        + list(BLAKE3_IV[:4])
        + [
            counter & 0xFFFFFFFF,
            (counter >> 32) & 0xFFFFFFFF,
            block_length,
            flags,
        ]
    )
    message = list(block_words)
    for round_index in range(7):
        blake3_mix(state, 0, 4, 8, 12, message[0], message[1])
        blake3_mix(state, 1, 5, 9, 13, message[2], message[3])
        blake3_mix(state, 2, 6, 10, 14, message[4], message[5])
        blake3_mix(state, 3, 7, 11, 15, message[6], message[7])
        blake3_mix(state, 0, 5, 10, 15, message[8], message[9])
        blake3_mix(state, 1, 6, 11, 12, message[10], message[11])
        blake3_mix(state, 2, 7, 8, 13, message[12], message[13])
        blake3_mix(state, 3, 4, 9, 14, message[14], message[15])
        if round_index != 6:
            message = [message[index] for index in BLAKE3_MESSAGE_PERMUTATION]
    return [state[index] ^ state[index + 8] for index in range(8)] + [
        state[index + 8] ^ int(chaining_value[index]) for index in range(8)
    ]


def blake3_single_chunk(value: bytes) -> str:
    """Return an unkeyed BLAKE3 digest for one chunk (the server hashes paths)."""
    if len(value) > 1024:
        raise ValidationError("canonical artifact root exceeds one BLAKE3 chunk")
    blocks = [value[index : index + 64] for index in range(0, len(value), 64)] or [b""]
    chaining_value: tuple[int, ...] | list[int] = BLAKE3_IV
    for index, block in enumerate(blocks[:-1]):
        words = list(struct.unpack("<16I", block.ljust(64, b"\0")))
        flags = BLAKE3_CHUNK_START if index == 0 else 0
        chaining_value = blake3_compress(chaining_value, words, 0, len(block), flags)[
            :8
        ]
    final = blocks[-1]
    final_words = list(struct.unpack("<16I", final.ljust(64, b"\0")))
    final_flags = BLAKE3_CHUNK_END | BLAKE3_ROOT
    if len(blocks) == 1:
        final_flags |= BLAKE3_CHUNK_START
    output = blake3_compress(chaining_value, final_words, 0, len(final), final_flags)
    return struct.pack("<16I", *output).hex()[:64]


def expected_repository_id(artifacts_root: Path) -> str:
    try:
        raw = str(artifacts_root).encode("utf-8", errors="strict")
    except UnicodeEncodeError as error:
        raise ValidationError("canonical artifact root is not UTF-8") from error
    return blake3_single_chunk(raw)[:16]


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def latest_complete_artifact(artifacts_root: Path) -> tuple[Path, dict[str, Any]]:
    candidates = [
        entry
        for entry in artifacts_root.iterdir()
        if entry.is_dir()
        and (entry / "nodes.jsonl").is_file()
        and (entry / "edges.jsonl").is_file()
    ]
    if not candidates:
        raise ValidationError(
            f"artifact root has no complete version: {text_hash(str(artifacts_root))}"
        )
    latest = max(
        candidates,
        key=lambda path: ((path / "nodes.jsonl").stat().st_mtime_ns, path.name),
    )
    nodes = latest / "nodes.jsonl"
    edges = latest / "edges.jsonl"
    sidecar = latest / "search-index.bin"
    if nodes.stat().st_size <= 0 or edges.stat().st_size <= 0:
        raise ValidationError("latest artifact has an empty nodes or edges file")
    if not sidecar.is_file() or sidecar.stat().st_size <= 0:
        raise ValidationError("latest artifact has no non-empty search-index.bin")
    return latest, {
        "nodes_sha256": file_sha256(nodes),
        "nodes_bytes": nodes.stat().st_size,
        "edges_bytes": edges.stat().st_size,
        "sidecar_bytes": sidecar.stat().st_size,
    }


def required_string(value: Any, field: str, index: int) -> str:
    if not isinstance(value, str) or not value.strip():
        raise ValidationError(f"manifest repository {index} has invalid {field}")
    return value


def load_manifest(path: Path) -> tuple[list[RepositorySpec], dict[str, Any]]:
    try:
        payload = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        raise ValidationError(
            f"cannot read soak manifest: {type(error).__name__}"
        ) from error
    if not isinstance(payload, dict) or payload.get("schema_version") != 1:
        raise ValidationError("soak manifest must be an object with schema_version=1")
    repositories = payload.get("repositories")
    if not isinstance(repositories, list) or len(repositories) != REQUIRED_REPOSITORIES:
        raise ValidationError(
            f"soak manifest must contain exactly {REQUIRED_REPOSITORIES} repositories"
        )

    specs: list[RepositorySpec] = []
    labels: set[str] = set()
    selectors: set[str] = set()
    roots: set[Path] = set()
    node_hashes: set[str] = set()
    for index, item in enumerate(repositories):
        if not isinstance(item, dict):
            raise ValidationError(f"manifest repository {index} is not an object")
        label = required_string(item.get("label"), "label", index)
        selector = required_string(item.get("repo"), "repo", index)
        artifacts_dir = required_string(
            item.get("artifacts_dir"), "artifacts_dir", index
        )
        search_query = required_string(item.get("search_query"), "search_query", index)
        grep_pattern = required_string(item.get("grep_pattern"), "grep_pattern", index)
        grep_glob = item.get("grep_glob", "")
        if not isinstance(grep_glob, str):
            raise ValidationError(f"manifest repository {index} has invalid grep_glob")
        try:
            root = Path(artifacts_dir).resolve(strict=True)
        except OSError as error:
            raise ValidationError(
                f"manifest repository {index} artifact root is unavailable"
            ) from error
        latest, artifact = latest_complete_artifact(root)
        if label in labels or selector in selectors or root in roots:
            raise ValidationError(
                "manifest labels, selectors, and canonical artifact roots must be distinct"
            )
        if artifact["nodes_sha256"] in node_hashes:
            raise ValidationError(
                "manifest repositories must have distinct nodes.jsonl content"
            )
        labels.add(label)
        selectors.add(selector)
        roots.add(root)
        node_hashes.add(artifact["nodes_sha256"])
        specs.append(
            RepositorySpec(
                label_hash=text_hash(label),
                selector=selector,
                artifacts_root=root,
                artifact_version=latest.name,
                expected_repository_id=expected_repository_id(root),
                nodes_sha256=artifact["nodes_sha256"],
                nodes_bytes=artifact["nodes_bytes"],
                edges_bytes=artifact["edges_bytes"],
                sidecar_bytes=artifact["sidecar_bytes"],
                search_query=search_query,
                grep_pattern=grep_pattern,
                grep_glob=grep_glob,
            )
        )

    return specs, {
        "repositories": len(specs),
        "distinct_labels": len(labels),
        "distinct_selectors": len(selectors),
        "distinct_artifact_roots": len(roots),
        "distinct_nodes_sha256": len(node_hashes),
        "nodes_bytes": sum(spec.nodes_bytes for spec in specs),
        "edges_bytes": sum(spec.edges_bytes for spec in specs),
        "sidecar_bytes": sum(spec.sidecar_bytes for spec in specs),
    }


def proc_start_time_ticks(pid: int) -> int:
    try:
        raw = Path(f"/proc/{pid}/stat").read_text()
    except OSError as error:
        raise ValidationError(
            f"cannot read server process stat: {type(error).__name__}"
        ) from error
    closing = raw.rfind(")")
    fields = raw[closing + 1 :].split() if closing >= 0 else []
    if len(fields) <= 19:
        raise ValidationError("server process stat is incomplete")
    try:
        return int(fields[19])
    except ValueError as error:
        raise ValidationError("server process start time is invalid") from error


def proc_environment(pid: int) -> dict[str, str]:
    try:
        raw = Path(f"/proc/{pid}/environ").read_bytes()
    except OSError as error:
        raise ValidationError(
            f"cannot read server process environment: {type(error).__name__}"
        ) from error
    wanted = {
        "CIH_SEARCH_CACHE_MAX_BYTES",
        "CIH_SEARCH_CACHE_MAX_ENTRIES",
        "CIH_PG_URL",
    }
    result: dict[str, str] = {}
    for entry in raw.split(b"\0"):
        if b"=" not in entry:
            continue
        key_raw, value_raw = entry.split(b"=", 1)
        key = key_raw.decode("utf-8", errors="replace")
        if key in wanted:
            result[key] = value_raw.decode("utf-8", errors="replace")
    return result


def positive_integer(value: str | None, default: int, name: str) -> int:
    if value is None:
        return default
    try:
        parsed = int(value)
    except ValueError as error:
        raise ValidationError(f"server {name} is not an integer") from error
    if parsed <= 0:
        raise ValidationError(f"server {name} must be positive")
    return parsed


def listening_socket_inodes(pid: int) -> set[str]:
    inodes: set[str] = set()
    try:
        descriptors = Path(f"/proc/{pid}/fd").iterdir()
        for descriptor in descriptors:
            try:
                target = os.readlink(descriptor)
            except OSError:
                continue
            if target.startswith("socket:[") and target.endswith("]"):
                inodes.add(target[8:-1])
    except OSError as error:
        raise ValidationError(
            f"cannot inspect server process descriptors: {type(error).__name__}"
        ) from error
    return inodes


def process_owns_listener(pid: int, port: int) -> bool:
    inodes = listening_socket_inodes(pid)
    if not inodes:
        return False
    try:
        lines = Path(f"/proc/{pid}/net/tcp").read_text().splitlines()[1:]
    except OSError:
        return False
    for line in lines:
        fields = line.split()
        if len(fields) <= 9 or fields[3] != "0A":
            continue
        try:
            local_address, port_hex = fields[1].rsplit(":", 1)
            local_port = int(port_hex, 16)
        except (ValueError, IndexError):
            continue
        # /proc/net/tcp renders IPv4 in host byte order: 127.0.0.1 is
        # 0100007F. A wildcard listener is also reachable through loopback.
        address_matches = local_address in {"0100007F", "00000000"}
        if address_matches and local_port == port and fields[9] in inodes:
            return True
    return False


def inspect_server_process(pid: int, server_url: str) -> ServerProcess:
    if sys.platform != "linux":
        raise ValidationError("production soak RSS acceptance requires Linux")
    try:
        command = Path(f"/proc/{pid}/cmdline").read_bytes().replace(b"\0", b" ")
        executable = Path(os.readlink(f"/proc/{pid}/exe")).name
    except OSError as error:
        raise ValidationError(
            f"cannot read server process command: {type(error).__name__}"
        ) from error
    if b"cih-server" not in command or executable != "cih-server":
        raise ValidationError("--server-pid does not identify a cih-server process")
    for namespace in ("pid", "net", "mnt"):
        try:
            runner_namespace = os.readlink(f"/proc/self/ns/{namespace}")
            server_namespace = os.readlink(f"/proc/{pid}/ns/{namespace}")
        except OSError as error:
            raise ValidationError(
                f"cannot inspect {namespace} namespace: {type(error).__name__}"
            ) from error
        if runner_namespace != server_namespace:
            raise ValidationError(
                f"runner and cih-server must share the {namespace} namespace"
            )
    parsed_url = urllib.parse.urlparse(server_url)
    if parsed_url.scheme != "http" or parsed_url.hostname != "127.0.0.1":
        raise ValidationError(
            "production soak requires a direct http://127.0.0.1 server URL"
        )
    try:
        listener_port = parsed_url.port or 80
    except ValueError as error:
        raise ValidationError("--server-url has an invalid port") from error
    if not process_owns_listener(pid, listener_port):
        raise ValidationError("--server-pid does not own the server URL listening port")
    environment = proc_environment(pid)
    return ServerProcess(
        pid=pid,
        start_time_ticks=proc_start_time_ticks(pid),
        cache_max_entries=positive_integer(
            environment.get("CIH_SEARCH_CACHE_MAX_ENTRIES"),
            DEFAULT_CACHE_MAX_ENTRIES,
            "CIH_SEARCH_CACHE_MAX_ENTRIES",
        ),
        cache_max_bytes=positive_integer(
            environment.get("CIH_SEARCH_CACHE_MAX_BYTES"),
            DEFAULT_CACHE_MAX_BYTES,
            "CIH_SEARCH_CACHE_MAX_BYTES",
        ),
        # Server config treats any present value, including an empty string, as
        # enabling the optional semantic provider. Acceptance requires it unset.
        semantic_enabled="CIH_PG_URL" in environment,
        listener_port=listener_port,
    )


def parse_proc_status(text: str) -> tuple[int, int]:
    values: dict[str, int] = {}
    for line in text.splitlines():
        if ":" not in line:
            continue
        key, raw = line.split(":", 1)
        if key not in {"VmRSS", "VmHWM"}:
            continue
        parts = raw.split()
        if not parts:
            continue
        try:
            kib = int(parts[0])
        except ValueError:
            continue
        values[key] = kib * 1024
    if "VmRSS" not in values or "VmHWM" not in values:
        raise ValidationError("server process status has no VmRSS/VmHWM")
    return values["VmRSS"], values["VmHWM"]


class ProcessSampler:
    def __init__(self, process: ServerProcess, interval_secs: float) -> None:
        self.process = process
        self.interval_secs = interval_secs
        self.samples: list[dict[str, float | int]] = []
        self.errors: list[str] = []
        self._started = 0.0
        self._stop = threading.Event()
        self._ready = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def start(self) -> None:
        self._started = time.perf_counter()
        self._thread.start()
        self._ready.wait(timeout=max(2.0, self.interval_secs))

    def stop(self) -> None:
        self._stop.set()
        self._thread.join(timeout=max(2.0, self.interval_secs + 1.0))
        if self._thread.is_alive():
            self.errors.append("sampler_stop_timeout")

    def _run(self) -> None:
        while not self._stop.is_set():
            try:
                if (
                    proc_start_time_ticks(self.process.pid)
                    != self.process.start_time_ticks
                ):
                    raise ValidationError("server process identity changed")
                status = Path(f"/proc/{self.process.pid}/status").read_text()
                rss_bytes, hwm_bytes = parse_proc_status(status)
                self.samples.append(
                    {
                        "elapsed_secs": time.perf_counter() - self._started,
                        "rss_bytes": rss_bytes,
                        "hwm_bytes": hwm_bytes,
                    }
                )
            except Exception as error:  # Acceptance records a sanitized failure class.
                if len(self.errors) < 20:
                    self.errors.append(type(error).__name__)
            finally:
                self._ready.set()
            self._stop.wait(self.interval_secs)


class HealthSampler:
    def __init__(self, http: Any, interval_secs: float = 1.0) -> None:
        self.http = http
        self.interval_secs = interval_secs
        self.latencies_ms: list[float] = []
        self.errors: list[str] = []
        self._stop = threading.Event()
        self._ready = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def start(self) -> None:
        self._thread.start()
        self._ready.wait(timeout=2.0)

    def stop(self) -> None:
        self._stop.set()
        self._thread.join(timeout=self.interval_secs + 2.0)
        if self._thread.is_alive():
            self.errors.append("sampler_stop_timeout")

    def _run(self) -> None:
        while not self._stop.is_set():
            try:
                started = time.perf_counter()
                self.http.get_json("/health")
                self.latencies_ms.append((time.perf_counter() - started) * 1000.0)
            except Exception as error:
                if len(self.errors) < 20:
                    self.errors.append(type(error).__name__)
            finally:
                self._ready.set()
            self._stop.wait(self.interval_secs)


def section_deltas(
    before: dict[str, Any], after: dict[str, Any], fields: tuple[str, ...]
) -> tuple[dict[str, int], list[str]]:
    deltas: dict[str, int] = {}
    resets: list[str] = []
    for field in fields:
        old = integer(before.get(field))
        new = integer(after.get(field))
        if new < old:
            resets.append(field)
            deltas[field] = 0
        else:
            deltas[field] = new - old
    return deltas, resets


def index_rows(metrics: Any) -> dict[tuple[str, str], dict[str, Any]]:
    rows = nested(metrics, "retrieval", "search_indexes", default=[])
    result: dict[tuple[str, str], dict[str, Any]] = {}
    if not isinstance(rows, list):
        return result
    for row in rows:
        if not isinstance(row, dict):
            continue
        repository_id = row.get("repository_id")
        artifact_version = row.get("artifact_version")
        if isinstance(repository_id, str) and isinstance(artifact_version, str):
            result[(repository_id, artifact_version)] = row
    return result


def result_digest(value: Any) -> str:
    return hashlib.sha256(canonical_result(value).encode("utf-8")).hexdigest()


def concurrent_search(
    client: Any, spec: RepositorySpec, callers: int
) -> dict[str, Any]:
    barrier = threading.Barrier(callers)

    def invoke() -> tuple[float, str, int]:
        barrier.wait(timeout=10.0)
        started = time.perf_counter()
        structured, _ = client.call_tool(
            "search_code",
            {"repo": spec.selector, "query": spec.search_query, "limit": 25},
        )
        elapsed_ms = (time.perf_counter() - started) * 1000.0
        count = len(structured) if isinstance(structured, list) else 0
        return elapsed_ms, result_digest(structured), count

    with concurrent.futures.ThreadPoolExecutor(max_workers=callers) as executor:
        samples = list(executor.map(lambda _: invoke(), range(callers)))
    durations = [sample[0] for sample in samples]
    digests = [sample[1] for sample in samples]
    counts = [sample[2] for sample in samples]
    return {
        "callers": callers,
        "p95_ms": percentile(durations, 0.95),
        "max_ms": max(durations, default=0.0),
        "all_results_equal": len(set(digests)) == 1,
        "all_results_non_empty": bool(counts) and min(counts) > 0,
        "result_digest": digests[0] if digests else "",
        "result_count": counts[0] if counts else 0,
    }


def one_search(client: Any, spec: RepositorySpec) -> tuple[float, str, int]:
    started = time.perf_counter()
    structured, _ = client.call_tool(
        "search_code",
        {"repo": spec.selector, "query": spec.search_query, "limit": 25},
    )
    elapsed_ms = (time.perf_counter() - started) * 1000.0
    count = len(structured) if isinstance(structured, list) else 0
    return elapsed_ms, result_digest(structured), count


def mixed_assignment(cycle: int, repository: int, count: int) -> str:
    if repository == cycle % count:
        return "grep_files"
    if repository == (cycle + 1) % count:
        return "architecture_overview"
    return "search_code"


def mixed_call(
    client: Any,
    spec: RepositorySpec,
    tool: str,
    barrier: threading.Barrier,
) -> dict[str, Any]:
    if tool == "search_code":
        arguments = {"repo": spec.selector, "query": spec.search_query, "limit": 25}
    elif tool == "architecture_overview":
        arguments = {
            "repo": spec.selector,
            "sections": ["stats", "modules", "route_groups", "entrypoints"],
            "limit": 25,
        }
    elif tool == "grep_files":
        arguments = {
            "repo": spec.selector,
            "pattern": spec.grep_pattern,
            "glob": spec.grep_glob,
            "limit": 50,
        }
    else:
        raise AssertionError(f"unexpected mixed tool {tool}")
    barrier.wait(timeout=10.0)
    started = time.perf_counter()
    try:
        structured, _ = client.call_tool(tool, arguments)
        return {
            "tool": tool,
            "elapsed_ms": (time.perf_counter() - started) * 1000.0,
            "digest": result_digest(structured) if tool == "search_code" else "",
            "error": "",
        }
    except Exception as error:
        return {
            "tool": tool,
            "elapsed_ms": (time.perf_counter() - started) * 1000.0,
            "digest": "",
            "error": type(error).__name__,
        }


def theil_sen_slope(points: list[tuple[float, float]]) -> float:
    slopes = [
        (right[1] - left[1]) / (right[0] - left[0])
        for index, left in enumerate(points)
        for right in points[index + 1 :]
        if right[0] > left[0]
    ]
    return float(statistics.median(slopes)) if slopes else 0.0


def maximum_growth_streak(values: list[float], step: float) -> int:
    current = 0
    maximum = 0
    for previous, observed in zip(values, values[1:]):
        if observed - previous > step:
            current += 1
            maximum = max(maximum, current)
        else:
            current = 0
    return maximum


def analyze_rss(
    samples: list[dict[str, float | int]],
    errors: list[str],
    duration_secs: float,
    warmup_secs: float,
    sample_interval_secs: float,
) -> dict[str, Any]:
    expected_samples = max(1, math.floor(duration_secs / sample_interval_secs))
    coverage = len(samples) / expected_samples
    after_warmup = [
        sample for sample in samples if float(sample["elapsed_secs"]) >= warmup_secs
    ]
    last_elapsed = max(
        (float(sample["elapsed_secs"]) for sample in after_warmup), default=warmup_secs
    )
    candidate_buckets = max(0, math.floor((last_elapsed - warmup_secs) / 60.0) + 1)
    minimum_per_bucket = max(1, math.ceil((60.0 / sample_interval_secs) * 0.8))
    bucket_points: list[tuple[float, float]] = []
    for bucket in range(candidate_buckets):
        lower = warmup_secs + bucket * 60.0
        upper = lower + 60.0
        values = [
            float(sample["rss_bytes"])
            for sample in after_warmup
            if lower <= float(sample["elapsed_secs"]) < upper
        ]
        if len(values) >= minimum_per_bucket:
            bucket_points.append((lower + 30.0, float(statistics.median(values))))

    bucket_values = [point[1] for point in bucket_points]
    baseline = (
        float(statistics.median(bucket_values[:3])) if len(bucket_values) >= 3 else 0.0
    )
    tail = (
        float(statistics.median(bucket_values[-3:])) if len(bucket_values) >= 3 else 0.0
    )
    drift_limit = max(32 * MIB, baseline * 0.05)
    trend_limit = max(16 * MIB, baseline * 0.02)
    growth_step = max(4 * MIB, baseline * 0.01)
    slope = theil_sen_slope(bucket_points)
    measured_span = (
        bucket_points[-1][0] - bucket_points[0][0] if len(bucket_points) >= 2 else 0.0
    )
    projected_growth = max(0.0, slope * measured_span)
    drift = tail - baseline
    growth_streak = maximum_growth_streak(bucket_values, growth_step)
    passed = (
        not errors
        and coverage >= 0.9
        and len(bucket_points) >= 20
        and drift <= drift_limit
        and projected_growth <= trend_limit
        and growth_streak < 5
    )
    return {
        "source": "/proc/<server-pid>/status",
        "samples": len(samples),
        "expected_samples": expected_samples,
        "sample_coverage": coverage,
        "sample_errors": errors[:20],
        "peak_rss_bytes": max(
            (integer(sample["rss_bytes"]) for sample in samples), default=0
        ),
        "peak_hwm_bytes": max(
            (integer(sample["hwm_bytes"]) for sample in samples), default=0
        ),
        "warmup_secs": warmup_secs,
        "complete_minute_buckets": len(bucket_points),
        "bucket_median_rss_bytes": [round(value) for value in bucket_values],
        "baseline_bytes": round(baseline),
        "tail_bytes": round(tail),
        "tail_drift_bytes": round(drift),
        "drift_limit_bytes": round(drift_limit),
        "theil_sen_bytes_per_second": slope,
        "projected_growth_bytes": round(projected_growth),
        "trend_limit_bytes": round(trend_limit),
        "growth_step_bytes": round(growth_step),
        "max_significant_growth_streak": growth_streak,
        "passed": passed,
    }


def latency_summary(values: list[float]) -> dict[str, Any]:
    return {
        "calls": len(values),
        "p50_ms": percentile(values, 0.50),
        "p95_ms": percentile(values, 0.95),
        "p99_ms": percentile(values, 0.99),
        "max_ms": max(values, default=0.0),
    }


def initial_report(args: argparse.Namespace) -> dict[str, Any]:
    return {
        "schema_version": 1,
        "generated_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "revision": args.revision,
        "production_eligible": args.duration_secs >= MINIMUM_DURATION_SECS,
        "configuration": {
            "duration_secs": args.duration_secs,
            "warmup_secs": args.warmup_secs,
            "sample_interval_secs": args.sample_interval_secs,
            "cold_callers": args.cold_callers,
            "server_pid": args.server_pid,
        },
        "gates": [],
    }


def require_condition(
    report: dict[str, Any], name: str, passed: bool, target: str, observed: str
) -> None:
    add_gate(report, name, passed, target, observed)
    if not passed:
        raise ValidationError(f"prerequisite failed: {name}")


def validate(args: argparse.Namespace, report: dict[str, Any]) -> dict[str, Any]:
    require_condition(
        report,
        "production_duration_configured",
        args.duration_secs >= MINIMUM_DURATION_SECS,
        f">= {MINIMUM_DURATION_SECS} seconds",
        f"{args.duration_secs} seconds",
    )
    require_condition(
        report,
        "rss_analysis_window",
        0 <= args.warmup_secs <= args.duration_secs - 20 * 60,
        "warmup leaves at least 20 minutes for RSS buckets",
        f"duration={args.duration_secs}, warmup={args.warmup_secs}",
    )
    repositories, artifact_report = load_manifest(args.manifest)
    report["artifact_preflight"] = artifact_report
    add_gate(
        report,
        "eight_distinct_production_artifacts",
        artifact_report["repositories"] == REQUIRED_REPOSITORIES
        and artifact_report["distinct_artifact_roots"] == REQUIRED_REPOSITORIES
        and artifact_report["distinct_nodes_sha256"] == REQUIRED_REPOSITORIES
        and artifact_report["sidecar_bytes"] > 0,
        "8 distinct selectors, roots, node hashes, and non-empty sidecars",
        (
            f"repos={artifact_report['repositories']}, "
            f"roots={artifact_report['distinct_artifact_roots']}, "
            f"node_hashes={artifact_report['distinct_nodes_sha256']}, "
            f"sidecar_bytes={artifact_report['sidecar_bytes']}"
        ),
    )

    process = inspect_server_process(args.server_pid, args.server_url)
    report["configuration"].update(
        {
            "search_cache_max_entries": process.cache_max_entries,
            "search_cache_max_bytes": process.cache_max_bytes,
            "semantic_enabled": process.semantic_enabled,
            "listener_port": process.listener_port,
        }
    )
    require_condition(
        report,
        "lexical_only_server",
        not process.semantic_enabled,
        "CIH_PG_URL absent from cih-server environment",
        f"semantic_enabled={process.semantic_enabled}",
    )

    token = os.environ.get(args.token_env)
    http = JsonHttpClient(args.server_url, token, args.request_timeout_secs)
    for path in ("/health", "/ready"):
        started = time.perf_counter()
        http.get_json(path)
        add_gate(
            report,
            f"{path[1:]}_endpoint",
            True,
            "HTTP success",
            f"{(time.perf_counter() - started) * 1000.0:.3f} ms",
        )

    client = McpClient(http)
    client.initialize()
    tools = client.list_tools()
    missing = sorted(set(MIXED_TOOLS) - set(tools))
    require_condition(
        report,
        "mixed_tool_discovery",
        not missing,
        "search_code, architecture_overview, and grep_files available",
        f"tool_count={len(tools)}, missing={missing}",
    )

    baseline_metrics = http.get_json("/operations/metrics")
    baseline_cache = search_cache(baseline_metrics)
    baseline_runtime = search_runtime(baseline_metrics)
    baseline_indexes = index_rows(baseline_metrics)
    fresh = (
        integer(baseline_cache.get("retained_entries")) == 0
        and integer(baseline_cache.get("builds")) == 0
        and integer(baseline_runtime.get("sidecar_loaded")) == 0
        and integer(baseline_runtime.get("fallback_builds")) == 0
        and not baseline_indexes
    )
    require_condition(
        report,
        "fresh_server_search_state",
        fresh,
        "zero retained/build/load/fallback counters and no observed indexes",
        (
            f"entries={integer(baseline_cache.get('retained_entries'))}, "
            f"builds={integer(baseline_cache.get('builds'))}, "
            f"loads={integer(baseline_runtime.get('sidecar_loaded'))}, "
            f"fallbacks={integer(baseline_runtime.get('fallback_builds'))}, "
            f"indexes={len(baseline_indexes)}"
        ),
    )

    mapped_rows: list[dict[str, Any]] = []
    baseline_digests: list[str] = []
    cold_results: list[dict[str, Any]] = []
    first_before = baseline_metrics
    for spec in repositories:
        before = http.get_json("/operations/metrics")
        before_rows = index_rows(before)
        burst = concurrent_search(client, spec, args.cold_callers)
        after = http.get_json("/operations/metrics")
        after_rows = index_rows(after)
        new_keys = sorted(set(after_rows) - set(before_rows))
        cache_delta, cache_resets = section_deltas(
            search_cache(before), search_cache(after), COUNTER_CACHE_FIELDS
        )
        runtime_delta, runtime_resets = section_deltas(
            search_runtime(before), search_runtime(after), COUNTER_RUNTIME_FIELDS
        )
        row = after_rows.get(new_keys[0], {}) if len(new_keys) == 1 else {}
        passed = (
            not cache_resets
            and not runtime_resets
            and burst["all_results_equal"]
            and burst["all_results_non_empty"]
            and cache_delta["builds"] == 1
            and cache_delta["evictions"] == 0
            and cache_delta["oversize"] == 0
            and runtime_delta["sidecar_loaded"] == 1
            and runtime_delta["fallback_builds"] == 0
            and len(new_keys) == 1
            and row.get("artifact_version") == spec.artifact_version
            and row.get("repository_id") == spec.expected_repository_id
            and integer(row.get("documents")) > 0
            and integer(row.get("index_bytes")) > 0
            and row.get("retained") is True
        )
        add_gate(
            report,
            f"cold_single_flight_{spec.label_hash}",
            passed,
            "one build/load, no fallback/eviction, identical non-empty results",
            (
                f"builds={cache_delta['builds']}, loads={runtime_delta['sidecar_loaded']}, "
                f"fallbacks={runtime_delta['fallback_builds']}, new_indexes={len(new_keys)}, "
                f"identity_match={row.get('repository_id') == spec.expected_repository_id}, "
                f"equal={burst['all_results_equal']}, non_empty={burst['all_results_non_empty']}"
            ),
        )
        if not passed:
            raise ValidationError(
                f"cold single-flight failed for repository {spec.label_hash}"
            )
        mapped_rows.append(row)
        baseline_digests.append(burst.pop("result_digest"))
        burst["repository"] = spec.label_hash
        cold_results.append(burst)

    first_after = http.get_json("/operations/metrics")
    first_cache_delta, first_cache_resets = section_deltas(
        search_cache(first_before), search_cache(first_after), COUNTER_CACHE_FIELDS
    )
    first_runtime_delta, first_runtime_resets = section_deltas(
        search_runtime(first_before),
        search_runtime(first_after),
        COUNTER_RUNTIME_FIELDS,
    )
    repository_ids = {row.get("repository_id") for row in mapped_rows}
    working_set_bytes = sum(integer(row.get("index_bytes")) for row in mapped_rows)
    required_cache_bytes = (working_set_bytes * 110 + 99) // 100
    first_cache = search_cache(first_after)
    first_passed = (
        not first_cache_resets
        and not first_runtime_resets
        and first_cache_delta["builds"] == REQUIRED_REPOSITORIES
        and first_cache_delta["evictions"] == 0
        and first_cache_delta["oversize"] == 0
        and first_runtime_delta["sidecar_loaded"] == REQUIRED_REPOSITORIES
        and first_runtime_delta["fallback_builds"] == 0
        and len(repository_ids) == REQUIRED_REPOSITORIES
        and integer(first_cache.get("retained_entries")) == REQUIRED_REPOSITORIES
        and integer(first_cache.get("retained_weight_bytes")) == working_set_bytes
        and process.cache_max_entries >= REQUIRED_REPOSITORIES
        and process.cache_max_bytes >= required_cache_bytes
    )
    add_gate(
        report,
        "production_hot_set_sized",
        first_passed,
        "8 retained distinct indexes and configured bytes >= working set + 10%",
        (
            f"ids={len(repository_ids)}, entries={integer(first_cache.get('retained_entries'))}, "
            f"retained_bytes={integer(first_cache.get('retained_weight_bytes'))}, "
            f"working_set_bytes={working_set_bytes}, required_bytes={required_cache_bytes}, "
            f"configured_bytes={process.cache_max_bytes}"
        ),
    )
    if not first_passed:
        raise ValidationError(
            "production hot set is not retained within configured policy"
        )

    second_before = first_after
    second_results: list[dict[str, Any]] = []
    for index, spec in enumerate(repositories):
        elapsed_ms, digest, result_count = one_search(client, spec)
        second_results.append(
            {
                "repository": spec.label_hash,
                "elapsed_ms": elapsed_ms,
                "result_count": result_count,
                "matches_first_pass": digest == baseline_digests[index],
            }
        )
    second_after = http.get_json("/operations/metrics")
    second_cache_delta, second_cache_resets = section_deltas(
        search_cache(second_before), search_cache(second_after), COUNTER_CACHE_FIELDS
    )
    second_runtime_delta, second_runtime_resets = section_deltas(
        search_runtime(second_before),
        search_runtime(second_after),
        COUNTER_RUNTIME_FIELDS,
    )
    second_cache = search_cache(second_after)
    second_passed = (
        not second_cache_resets
        and not second_runtime_resets
        and second_cache_delta["hits"] >= REQUIRED_REPOSITORIES
        and second_cache_delta["builds"] == 0
        and second_cache_delta["evictions"] == 0
        and second_cache_delta["oversize"] == 0
        and second_runtime_delta["sidecar_loaded"] == 0
        and second_runtime_delta["fallback_builds"] == 0
        and all(result["matches_first_pass"] for result in second_results)
        and integer(second_cache.get("retained_entries")) == REQUIRED_REPOSITORIES
        and integer(second_cache.get("retained_weight_bytes")) == working_set_bytes
    )
    add_gate(
        report,
        "alternating_hot_set_second_pass",
        second_passed,
        "all hits; no load/build/eviction; rankings match first pass",
        (
            f"hits={second_cache_delta['hits']}, builds={second_cache_delta['builds']}, "
            f"loads={second_runtime_delta['sidecar_loaded']}, "
            f"evictions={second_cache_delta['evictions']}, "
            f"matching={sum(result['matches_first_pass'] for result in second_results)}/8"
        ),
    )
    if not second_passed:
        raise ValidationError("alternating hot-set second pass failed")

    report["hot_set"] = {
        "repositories": [
            {
                "label_hash": spec.label_hash,
                "repository_id": row.get("repository_id"),
                "artifact_version": row.get("artifact_version"),
                "documents": integer(row.get("documents")),
                "index_bytes": integer(row.get("index_bytes")),
                "retained": row.get("retained") is True,
            }
            for spec, row in zip(repositories, mapped_rows)
        ],
        "working_set_bytes": working_set_bytes,
        "required_cache_bytes_with_headroom": required_cache_bytes,
        "first_pass_delta": {
            "search_cache": first_cache_delta,
            "search_runtime": first_runtime_delta,
        },
        "second_pass_delta": {
            "search_cache": second_cache_delta,
            "search_runtime": second_runtime_delta,
        },
        "cold_searches": cold_results,
        "second_searches": second_results,
    }

    soak_before = second_after
    process_sampler = ProcessSampler(process, args.sample_interval_secs)
    health_http = JsonHttpClient(
        args.server_url, token, min(2.0, args.request_timeout_secs)
    )
    health_sampler = HealthSampler(health_http)
    latencies: dict[str, list[float]] = {tool: [] for tool in MIXED_TOOLS}
    calls_by_repository: list[dict[str, int]] = [
        {tool: 0 for tool in MIXED_TOOLS} for _ in repositories
    ]
    tool_errors: list[dict[str, str]] = []
    search_hash_mismatches = 0
    successful_searches = 0
    cycles = 0
    process_sampler.start()
    health_sampler.start()
    soak_started = time.perf_counter()
    soak_finished = soak_started
    try:
        deadline = soak_started + args.duration_secs
        with concurrent.futures.ThreadPoolExecutor(
            max_workers=REQUIRED_REPOSITORIES
        ) as executor:
            while time.perf_counter() < deadline and len(tool_errors) < args.max_errors:
                barrier = threading.Barrier(REQUIRED_REPOSITORIES)
                futures = []
                assignments: list[str] = []
                for index, spec in enumerate(repositories):
                    tool = mixed_assignment(cycles, index, len(repositories))
                    assignments.append(tool)
                    futures.append(
                        executor.submit(mixed_call, client, spec, tool, barrier)
                    )
                results = [future.result() for future in futures]
                for index, (tool, result) in enumerate(zip(assignments, results)):
                    latencies[tool].append(float(result["elapsed_ms"]))
                    if result["error"]:
                        tool_errors.append(
                            {
                                "repository": repositories[index].label_hash,
                                "tool": tool,
                                "error": str(result["error"]),
                            }
                        )
                        continue
                    calls_by_repository[index][tool] += 1
                    if tool == "search_code":
                        successful_searches += 1
                        if result["digest"] != baseline_digests[index]:
                            search_hash_mismatches += 1
                cycles += 1
    finally:
        soak_finished = time.perf_counter()
        health_sampler.stop()
        process_sampler.stop()
    actual_duration_secs = soak_finished - soak_started
    soak_after = http.get_json("/operations/metrics")
    soak_cache_delta, soak_cache_resets = section_deltas(
        search_cache(soak_before), search_cache(soak_after), COUNTER_CACHE_FIELDS
    )
    soak_runtime_delta, soak_runtime_resets = section_deltas(
        search_runtime(soak_before), search_runtime(soak_after), COUNTER_RUNTIME_FIELDS
    )
    final_cache = search_cache(soak_after)
    latency = {tool: latency_summary(values) for tool, values in latencies.items()}
    rss = analyze_rss(
        process_sampler.samples,
        process_sampler.errors,
        actual_duration_secs,
        args.warmup_secs,
        args.sample_interval_secs,
    )
    expected_health_samples = max(1, math.floor(actual_duration_secs))
    health_coverage = len(health_sampler.latencies_ms) / expected_health_samples
    health = {
        "samples": len(health_sampler.latencies_ms),
        "expected_samples": expected_health_samples,
        "sample_coverage": health_coverage,
        "p99_ms": percentile(health_sampler.latencies_ms, 0.99),
        "max_ms": max(health_sampler.latencies_ms, default=0.0),
        "errors": health_sampler.errors[:20],
    }
    report["soak"] = {
        "actual_duration_secs": actual_duration_secs,
        "cycles": cycles,
        "successful_searches": successful_searches,
        "search_hash_mismatches": search_hash_mismatches,
        "tool_errors": tool_errors[: args.max_errors],
        "latency_ms": latency,
        "calls_by_repository": [
            {"label_hash": spec.label_hash, **counts}
            for spec, counts in zip(repositories, calls_by_repository)
        ],
        "health": health,
        "search_cache_delta": soak_cache_delta,
        "search_runtime_delta": soak_runtime_delta,
        "counter_resets": {
            "search_cache": soak_cache_resets,
            "search_runtime": soak_runtime_resets,
        },
        "rss": rss,
    }

    add_gate(
        report,
        "mixed_soak_duration",
        actual_duration_secs >= args.duration_secs,
        f">= configured {args.duration_secs} seconds",
        f"{actual_duration_secs:.3f} seconds, cycles={cycles}",
    )
    mixed_coverage = all(
        all(counts[tool] > 0 for tool in MIXED_TOOLS) for counts in calls_by_repository
    )
    add_gate(
        report,
        "mixed_soak_tool_coverage",
        mixed_coverage and not tool_errors,
        "every repository runs every tool with zero errors",
        f"coverage={mixed_coverage}, errors={len(tool_errors)}",
    )
    add_gate(
        report,
        "mixed_soak_latency",
        latency["search_code"]["calls"] > 0
        and latency["search_code"]["p95_ms"] <= 500.0
        and latency["architecture_overview"]["calls"] > 0
        and latency["architecture_overview"]["p95_ms"] <= 2_000.0
        and latency["grep_files"]["calls"] > 0
        and latency["grep_files"]["p95_ms"] <= 10_000.0,
        "search p95 <=500 ms; overview <=2000 ms; grep <=10000 ms",
        (
            f"search={latency['search_code']['p95_ms']:.3f}, "
            f"overview={latency['architecture_overview']['p95_ms']:.3f}, "
            f"grep={latency['grep_files']['p95_ms']:.3f} ms"
        ),
    )
    add_gate(
        report,
        "mixed_soak_health",
        not health["errors"]
        and health["sample_coverage"] >= 0.9
        and health["p99_ms"] < 50.0,
        "health p99 <50 ms and >=90% sample coverage with no errors",
        (
            f"p99={health['p99_ms']:.3f} ms, coverage={health['sample_coverage']:.3f}, "
            f"errors={len(health['errors'])}"
        ),
    )
    no_hot_work = (
        not soak_cache_resets
        and not soak_runtime_resets
        and soak_cache_delta["builds"] == 0
        and soak_cache_delta["evictions"] == 0
        and soak_cache_delta["oversize"] == 0
        and soak_runtime_delta["sidecar_loaded"] == 0
        and soak_runtime_delta["fallback_builds"] == 0
        and soak_cache_delta["hits"] == successful_searches
        and integer(final_cache.get("retained_entries")) == REQUIRED_REPOSITORIES
        and integer(final_cache.get("retained_weight_bytes")) == working_set_bytes
        and search_hash_mismatches == 0
    )
    add_gate(
        report,
        "mixed_soak_no_duplicate_or_evicted_work",
        no_hot_work,
        "no counter resets/load/build/eviction and every search is a stable cache hit",
        (
            f"hits={soak_cache_delta['hits']}/{successful_searches}, "
            f"builds={soak_cache_delta['builds']}, loads={soak_runtime_delta['sidecar_loaded']}, "
            f"fallbacks={soak_runtime_delta['fallback_builds']}, "
            f"evictions={soak_cache_delta['evictions']}, mismatches={search_hash_mismatches}, "
            f"resets={len(soak_cache_resets) + len(soak_runtime_resets)}"
        ),
    )
    add_gate(
        report,
        "mixed_soak_rss_stable",
        rss["passed"],
        "RSS samples present; tail/trend/streak within documented limits",
        (
            f"samples={rss['samples']}, buckets={rss['complete_minute_buckets']}, "
            f"drift={rss['tail_drift_bytes']}/{rss['drift_limit_bytes']}, "
            f"projected={rss['projected_growth_bytes']}/{rss['trend_limit_bytes']}, "
            f"streak={rss['max_significant_growth_streak']}"
        ),
    )
    report["passed"] = all(gate["passed"] for gate in report["gates"])
    return report


def run_self_test() -> None:
    import tempfile

    assert blake3_single_chunk(b"") == (
        "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
    )
    assert blake3_single_chunk(b"abc") == (
        "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
    )
    assert blake3_single_chunk(bytes(index % 251 for index in range(1024))) == (
        "42214739f095a406f3fc83deb889744ac00df831c10daa55189b5d121c855af7"
    )
    rss, hwm = parse_proc_status("VmRSS:\t123 kB\nVmHWM:\t456 kB\n")
    assert rss == 123 * 1024
    assert hwm == 456 * 1024
    deltas, resets = section_deltas(
        {"hits": 10, "builds": 2},
        {"hits": 12, "builds": 1},
        ("hits", "builds"),
    )
    assert deltas == {"hits": 2, "builds": 0}
    assert resets == ["builds"]

    coverage = [
        {
            mixed_assignment(cycle, repository, REQUIRED_REPOSITORIES)
            for cycle in range(8)
        }
        for repository in range(REQUIRED_REPOSITORIES)
    ]
    assert all(tools == set(MIXED_TOOLS) for tools in coverage)
    assert maximum_growth_streak([100.0, 106.0, 113.0, 120.0, 128.0, 137.0], 5.0) == 5
    assert theil_sen_slope([(0.0, 10.0), (1.0, 12.0), (2.0, 14.0)]) == 2.0

    if sys.platform == "linux":
        import socket

        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen()
            assert process_owns_listener(os.getpid(), listener.getsockname()[1])
        assert proc_start_time_ticks(os.getpid()) > 0

    with tempfile.TemporaryDirectory(prefix="cih-production-soak-self-test-") as temp:
        base = Path(temp)
        manifest_repositories = []
        for index in range(REQUIRED_REPOSITORIES):
            artifacts = base / f"repo-{index}" / ".cih" / "artifacts"
            version = artifacts / f"version-{index}"
            version.mkdir(parents=True)
            (version / "nodes.jsonl").write_text(
                json.dumps({"id": f"node-{index}"}) + "\n"
            )
            (version / "edges.jsonl").write_text(
                json.dumps({"source": f"node-{index}", "target": f"node-{index}"})
                + "\n"
            )
            (version / "search-index.bin").write_bytes(bytes([index + 1]))
            manifest_repositories.append(
                {
                    "label": f"repo-{index}",
                    "repo": f"selector-{index}",
                    "artifacts_dir": str(artifacts),
                    "search_query": f"node-{index}",
                    "grep_pattern": f"node-{index}",
                    "grep_glob": "",
                }
            )
        manifest = base / "manifest.json"
        manifest.write_text(
            json.dumps({"schema_version": 1, "repositories": manifest_repositories})
        )
        specs, artifact_report = load_manifest(manifest)
        assert len(specs) == REQUIRED_REPOSITORIES
        assert artifact_report["distinct_nodes_sha256"] == REQUIRED_REPOSITORIES
        assert (
            len({spec.expected_repository_id for spec in specs})
            == REQUIRED_REPOSITORIES
        )

    stable_samples = [
        {
            "elapsed_secs": float(second),
            "rss_bytes": 500 * MIB + ((second // 5) % 3) * MIB,
            "hwm_bytes": 503 * MIB,
        }
        for second in range(0, 25 * 60 + 1, 5)
    ]
    stable = analyze_rss(stable_samples, [], 25 * 60, 0, 5)
    assert stable["passed"]
    minimum_window = analyze_rss(stable_samples[:240], [], 20 * 60, 0, 5)
    assert minimum_window["complete_minute_buckets"] == 20
    assert minimum_window["passed"]
    missing = analyze_rss([], ["ValidationError"], 25 * 60, 0, 5)
    assert not missing["passed"]
    print("self-test passed")


def current_revision() -> str:
    return SUPPORT.current_revision()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server-url", default="http://127.0.0.1:8080")
    parser.add_argument(
        "--server-pid",
        type=int,
        default=int(os.environ.get("CIH_ACCEPT_SERVER_PID", "0")),
        help="cih-server PID visible from this Linux PID namespace",
    )
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--duration-secs", type=int, default=MINIMUM_DURATION_SECS)
    parser.add_argument("--warmup-secs", type=int, default=DEFAULT_WARMUP_SECS)
    parser.add_argument("--sample-interval-secs", type=float, default=5.0)
    parser.add_argument("--cold-callers", type=int, default=16)
    parser.add_argument("--request-timeout-secs", type=float, default=95.0)
    parser.add_argument("--max-errors", type=int, default=10)
    parser.add_argument("--token-env", default="CIH_API_TOKEN")
    parser.add_argument(
        "--output",
        type=Path,
        default=Path("docs/perf/retrieval-production-soak.json"),
    )
    parser.add_argument("--revision", default=current_revision())
    parser.add_argument("--self-test", action="store_true")
    return parser.parse_args()


def validate_cli(args: argparse.Namespace) -> None:
    if args.manifest is None:
        raise ValidationError("--manifest is required")
    if args.server_pid <= 0:
        raise ValidationError("--server-pid must be positive")
    if args.duration_secs <= 0 or args.warmup_secs < 0:
        raise ValidationError("duration must be positive and warmup non-negative")
    if args.sample_interval_secs <= 0:
        raise ValidationError("--sample-interval-secs must be positive")
    if args.request_timeout_secs <= 0:
        raise ValidationError("--request-timeout-secs must be positive")
    if args.cold_callers < 2:
        raise ValidationError("--cold-callers must be at least 2")
    if args.max_errors <= 0:
        raise ValidationError("--max-errors must be positive")


def print_summary(report: dict[str, Any], output: Path) -> None:
    for gate in report.get("gates", []):
        status = "PASS" if gate.get("passed") else "FAIL"
        print(f"[{status}] {gate.get('name')}: {gate.get('observed')}")
    if report.get("error"):
        print(f"[FAIL] validation_error: {report['error']}")
    print(f"report: {output}")


def main() -> int:
    args = parse_args()
    if args.self_test:
        run_self_test()
        return 0
    report = initial_report(args)
    try:
        validate_cli(args)
        report = validate(args, report)
    except Exception as error:
        report["passed"] = False
        report["error"] = (
            f"{type(error).__name__}:sha256:{text_hash(str(error), length=24)}"
        )
    write_report(args.output, report)
    print_summary(report, args.output)
    return 0 if report.get("passed") is True else 1


if __name__ == "__main__":
    sys.exit(main())
