#!/usr/bin/env python3
"""Validate CIH retrieval against a running production-scale MCP server.

Run this immediately after restarting the server so the first search burst is
actually cold. The report stores timings, counters, and completeness metadata;
it deliberately omits tool result bodies and source text.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import datetime as dt
import json
import math
import os
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any


MIB = 1024 * 1024
SEARCH_INDEX_LIMIT_BYTES = 230 * MIB
SCORER_TOTAL_SCRATCH_LIMIT_BYTES = 32 * MIB
SCORER_PER_ACTIVE_LIMIT_BYTES = 6 * MIB
SEARCH_CONCURRENCY = 16


class ValidationError(RuntimeError):
    pass


@dataclass(frozen=True)
class HttpResult:
    payload: Any
    body_bytes: int
    headers: Any


class JsonHttpClient:
    def __init__(self, base_url: str, token: str | None, timeout: float) -> None:
        normalized = base_url.rstrip("/")
        if normalized.endswith("/mcp"):
            normalized = normalized[:-4]
        self.base_url = normalized
        self.token = token
        self.timeout = timeout

    def request(
        self,
        method: str,
        path: str,
        payload: dict[str, Any] | None = None,
        session_id: str | None = None,
    ) -> HttpResult:
        body = None if payload is None else json.dumps(payload).encode("utf-8")
        headers = {"Accept": "application/json, text/event-stream"}
        if body is not None:
            headers["Content-Type"] = "application/json"
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        if session_id:
            headers["Mcp-Session-Id"] = session_id
        request = urllib.request.Request(
            urllib.parse.urljoin(f"{self.base_url}/", path.lstrip("/")),
            data=body,
            headers=headers,
            method=method,
        )
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                raw = response.read()
                content_type = response.headers.get("Content-Type", "")
                parsed = parse_http_payload(raw, content_type)
                return HttpResult(parsed, len(raw), response.headers)
        except urllib.error.HTTPError as error:
            raw = error.read()
            detail = raw.decode("utf-8", errors="replace")[:500]
            raise ValidationError(
                f"{method} {path} returned HTTP {error.code}: {detail}"
            ) from error
        except urllib.error.URLError as error:
            raise ValidationError(f"{method} {path} failed: {error.reason}") from error

    def get_json(self, path: str) -> Any:
        return self.request("GET", path).payload


class McpClient:
    def __init__(self, http: JsonHttpClient) -> None:
        self.http = http
        self.session_id: str | None = None
        self._next_id = 1
        self._id_lock = threading.Lock()

    def initialize(self) -> None:
        request_id = self._request_id()
        initialized = self.http.request(
            "POST",
            "/mcp",
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "cih-production-acceptance",
                        "version": "1",
                    },
                },
            },
        )
        require_rpc_success(initialized.payload, request_id)
        self.session_id = initialized.headers.get("Mcp-Session-Id")
        if not self.session_id:
            raise ValidationError("initialize response did not include Mcp-Session-Id")
        self.http.request(
            "POST",
            "/mcp",
            {
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
                "params": {},
            },
            self.session_id,
        )

    def list_tools(self) -> list[str]:
        response, _ = self._call("tools/list", {})
        result = require_rpc_success(response)
        tools = result.get("tools", []) if isinstance(result, dict) else []
        return sorted(
            tool.get("name", "")
            for tool in tools
            if isinstance(tool, dict) and tool.get("name")
        )

    def call_tool(self, name: str, arguments: dict[str, Any]) -> tuple[Any, int]:
        response, body_bytes = self._call(
            "tools/call", {"name": name, "arguments": arguments}
        )
        result = require_rpc_success(response)
        if not isinstance(result, dict):
            raise ValidationError(f"{name} returned a non-object MCP result")
        if result.get("isError") is True:
            raise ValidationError(f"{name} returned isError=true")
        structured = result.get("structuredContent", result.get("structured_content"))
        if structured is None:
            structured = structured_from_content(result.get("content"))
        return structured, body_bytes

    def _call(self, method: str, params: dict[str, Any]) -> tuple[Any, int]:
        if not self.session_id:
            raise ValidationError("MCP client is not initialized")
        request_id = self._request_id()
        response = self.http.request(
            "POST",
            "/mcp",
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": params,
            },
            self.session_id,
        )
        require_rpc_success(response.payload, request_id)
        return response.payload, response.body_bytes

    def _request_id(self) -> int:
        with self._id_lock:
            request_id = self._next_id
            self._next_id += 1
            return request_id


class RuntimeMonitor:
    def __init__(self, http: JsonHttpClient) -> None:
        self.http = http
        self.health_ms: list[float] = []
        self.errors: list[str] = []
        self.max_scorer_active = 0
        self.max_scorer_scratch_bytes = 0
        self.max_scratch_per_active_bytes = 0
        self.max_cold_reserved_bytes = 0
        self._stop = threading.Event()
        self._ready = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def start(self) -> None:
        self._thread.start()
        self._ready.wait(timeout=2.0)

    def stop(self) -> None:
        self._stop.set()
        self._thread.join(timeout=2.0)
        if self._thread.is_alive():
            self.errors.append("runtime monitor did not stop within 2 seconds")
            return
        self._sample()

    def report(self) -> dict[str, Any]:
        return {
            "health_samples": len(self.health_ms),
            "health_p99_ms": percentile(self.health_ms, 0.99),
            "health_max_ms": max(self.health_ms, default=0.0),
            "monitor_errors": self.errors[:5],
            "max_scorer_active": self.max_scorer_active,
            "max_scorer_scratch_bytes": self.max_scorer_scratch_bytes,
            "max_scratch_per_active_bytes": self.max_scratch_per_active_bytes,
            "max_cold_reserved_bytes": self.max_cold_reserved_bytes,
        }

    def _run(self) -> None:
        while not self._stop.is_set():
            self._sample()
            self._stop.wait(0.005)

    def _sample(self) -> None:
        try:
            started = time.perf_counter()
            self.http.get_json("/health")
            self.health_ms.append((time.perf_counter() - started) * 1000.0)
            metrics = self.http.get_json("/operations/metrics")
            runtime = nested(metrics, "retrieval", "search_runtime", default={})
            active = max(
                integer(runtime.get("scorer_active")),
                integer(runtime.get("scorer_peak_active")),
            )
            scratch = max(
                integer(runtime.get("scorer_scratch_bytes")),
                integer(runtime.get("scorer_peak_scratch_bytes")),
            )
            per_query_scratch = integer(
                runtime.get("scorer_peak_per_query_scratch_bytes")
            )
            self.max_scorer_active = max(self.max_scorer_active, active)
            self.max_scorer_scratch_bytes = max(self.max_scorer_scratch_bytes, scratch)
            if per_query_scratch > 0:
                self.max_scratch_per_active_bytes = max(
                    self.max_scratch_per_active_bytes, per_query_scratch
                )
            elif active > 0:
                self.max_scratch_per_active_bytes = max(
                    self.max_scratch_per_active_bytes, scratch // active
                )
            self.max_cold_reserved_bytes = max(
                self.max_cold_reserved_bytes,
                integer(runtime.get("cold_reserved_bytes")),
            )
        except Exception as error:  # The final gate reports sampling failures.
            if len(self.errors) < 5:
                self.errors.append(str(error)[:300])
        finally:
            self._ready.set()


def parse_http_payload(raw: bytes, content_type: str) -> Any:
    if not raw:
        return None
    text = raw.decode("utf-8", errors="strict")
    if "text/event-stream" not in content_type:
        return json.loads(text)
    events: list[Any] = []
    data_lines: list[str] = []
    for line in text.splitlines():
        if not line:
            if data_lines:
                events.append(json.loads("\n".join(data_lines)))
                data_lines.clear()
            continue
        if line.startswith("data:"):
            data_lines.append(line[5:].lstrip())
    if data_lines:
        events.append(json.loads("\n".join(data_lines)))
    if not events:
        raise ValidationError("event-stream response contained no JSON data event")
    return events[-1]


def require_rpc_success(payload: Any, expected_id: int | None = None) -> Any:
    if not isinstance(payload, dict):
        raise ValidationError("MCP response was not a JSON object")
    if expected_id is not None and payload.get("id") != expected_id:
        raise ValidationError(
            f"MCP response id {payload.get('id')!r} did not match {expected_id}"
        )
    if "error" in payload:
        error = payload["error"]
        if isinstance(error, dict):
            message = str(error.get("message", "MCP request failed"))[:500]
        else:
            message = str(error)[:500]
        raise ValidationError(message)
    return payload.get("result")


def structured_from_content(content: Any) -> Any:
    if not isinstance(content, list):
        return None
    for item in content:
        if not isinstance(item, dict):
            continue
        if item.get("type") == "text" and isinstance(item.get("text"), str):
            try:
                return json.loads(item["text"])
            except json.JSONDecodeError:
                continue
    return None


def nested(value: Any, *keys: str, default: Any = None) -> Any:
    current = value
    for key in keys:
        if not isinstance(current, dict) or key not in current:
            return default
        current = current[key]
    return current


def integer(value: Any) -> int:
    return value if isinstance(value, int) and not isinstance(value, bool) else 0


def percentile(values: list[float], fraction: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    rank = max(1, math.ceil(len(ordered) * fraction))
    return ordered[rank - 1]


def summarize_result(value: Any) -> dict[str, Any]:
    if isinstance(value, list):
        return {"result_type": "list", "result_count": len(value)}
    if not isinstance(value, dict):
        return {"result_type": type(value).__name__}
    safe_fields = (
        "complete",
        "truncated",
        "truncation_reason",
        "matches_returned",
        "candidate_files",
        "files_scanned",
        "files_skipped",
        "elapsed_ms",
    )
    summary = {"result_type": "object", "top_level_keys": sorted(value.keys())}
    for field in safe_fields:
        if field in value and isinstance(
            value[field], (bool, int, float, str, type(None))
        ):
            summary[field] = value[field]
    return summary


def canonical_result(value: Any) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True)


def concurrent_tool_calls(
    client: McpClient,
    name: str,
    arguments: dict[str, Any],
    callers: int,
    http: JsonHttpClient,
) -> dict[str, Any]:
    barrier = threading.Barrier(callers)
    monitor = RuntimeMonitor(http)

    def invoke() -> tuple[float, Any, int]:
        barrier.wait(timeout=10.0)
        started = time.perf_counter()
        structured, body_bytes = client.call_tool(name, arguments)
        return (time.perf_counter() - started) * 1000.0, structured, body_bytes

    monitor.start()
    wall_started = time.perf_counter()
    try:
        with concurrent.futures.ThreadPoolExecutor(max_workers=callers) as executor:
            samples = list(executor.map(lambda _: invoke(), range(callers)))
    finally:
        monitor.stop()
    wall_ms = (time.perf_counter() - wall_started) * 1000.0
    durations = [sample[0] for sample in samples]
    bodies = [sample[2] for sample in samples]
    canonical = [canonical_result(sample[1]) for sample in samples]
    return {
        "callers": callers,
        "wall_ms": wall_ms,
        "p50_ms": percentile(durations, 0.50),
        "p95_ms": percentile(durations, 0.95),
        "p99_ms": percentile(durations, 0.99),
        "max_ms": max(durations, default=0.0),
        "max_response_bytes": max(bodies, default=0),
        "all_results_equal": len(set(canonical)) <= 1,
        "result_summary": summarize_result(samples[0][1]) if samples else {},
        "runtime_monitor": monitor.report(),
    }


def one_tool_call(
    client: McpClient, name: str, arguments: dict[str, Any]
) -> dict[str, Any]:
    started = time.perf_counter()
    structured, body_bytes = client.call_tool(name, arguments)
    return {
        "elapsed_ms": (time.perf_counter() - started) * 1000.0,
        "response_bytes": body_bytes,
        "result_summary": summarize_result(structured),
    }


def search_runtime(metrics: Any) -> dict[str, Any]:
    value = nested(metrics, "retrieval", "search_runtime", default={})
    return value if isinstance(value, dict) else {}


def search_cache(metrics: Any) -> dict[str, Any]:
    value = nested(metrics, "retrieval", "search_cache", default={})
    return value if isinstance(value, dict) else {}


def counter_delta(before: dict[str, Any], after: dict[str, Any], key: str) -> int:
    return max(0, integer(after.get(key)) - integer(before.get(key)))


def latest_artifact(artifacts_dir: Path) -> dict[str, Any]:
    if not artifacts_dir.is_dir():
        raise ValidationError(f"artifact root does not exist: {artifacts_dir}")
    candidates = [
        entry
        for entry in artifacts_dir.iterdir()
        if entry.is_dir()
        and (entry / "nodes.jsonl").is_file()
        and (entry / "edges.jsonl").is_file()
    ]
    if not candidates:
        raise ValidationError("artifact root has no complete version directory")
    latest = max(
        candidates,
        key=lambda path: ((path / "nodes.jsonl").stat().st_mtime_ns, path.name),
    )
    sidecar = latest / "search-index.bin"
    return {
        "artifact_version": latest.name,
        "nodes_bytes": (latest / "nodes.jsonl").stat().st_size,
        "sidecar_present": sidecar.is_file(),
        "sidecar_bytes": sidecar.stat().st_size if sidecar.is_file() else 0,
    }


def add_gate(
    report: dict[str, Any], name: str, passed: bool, target: str, observed: str
) -> None:
    report.setdefault("gates", []).append(
        {
            "name": name,
            "passed": bool(passed),
            "target": target,
            "observed": observed,
        }
    )


def repo_arguments(selector: str, **arguments: Any) -> dict[str, Any]:
    return {**arguments, "repo": selector}


def initial_report(args: argparse.Namespace) -> dict[str, Any]:
    return {
        "generated_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "server_url": args.server_url,
        "repository_label": args.repository_label,
        "repo_selector": args.repo or "<primary>",
        "revision": args.revision,
        "gates": [],
    }


def validate(args: argparse.Namespace, report: dict[str, Any]) -> dict[str, Any]:
    token = os.environ.get(args.token_env)
    http = JsonHttpClient(args.server_url, token, args.request_timeout_secs)

    artifact = latest_artifact(args.artifacts_dir)
    report["artifact"] = artifact
    add_gate(
        report,
        "search_sidecar_present",
        artifact["sidecar_present"] and artifact["sidecar_bytes"] > 0,
        "non-empty search-index.bin in latest complete artifact",
        f"present={artifact['sidecar_present']}, bytes={artifact['sidecar_bytes']}",
    )

    for path in ("/health", "/ready"):
        started = time.perf_counter()
        http.get_json(path)
        elapsed_ms = (time.perf_counter() - started) * 1000.0
        report[path.lstrip("/")] = {"elapsed_ms": elapsed_ms, "ok": True}
        add_gate(
            report, f"{path[1:]}_endpoint", True, "HTTP success", f"{elapsed_ms:.3f} ms"
        )

    client = McpClient(http)
    client.initialize()
    tools = client.list_tools()
    required_tools = {"search_code", "architecture_overview", "grep_files"}
    missing_tools = sorted(required_tools - set(tools))
    report["tool_discovery"] = {"tool_count": len(tools), "missing": missing_tools}
    add_gate(
        report,
        "mcp_tool_discovery",
        bool(tools) and not missing_tools,
        "non-empty list containing search_code, architecture_overview, grep_files",
        f"count={len(tools)}, missing={missing_tools}",
    )

    before_cold = http.get_json("/operations/metrics")
    cold = concurrent_tool_calls(
        client,
        "search_code",
        repo_arguments(args.repo, query=args.search_query, limit=25),
        SEARCH_CONCURRENCY,
        http,
    )
    after_cold = http.get_json("/operations/metrics")
    cold_before_runtime = search_runtime(before_cold)
    cold_after_runtime = search_runtime(after_cold)
    cold["sidecar_loads"] = counter_delta(
        cold_before_runtime, cold_after_runtime, "sidecar_loaded"
    )
    cold["fallback_builds"] = counter_delta(
        cold_before_runtime, cold_after_runtime, "fallback_builds"
    )
    cold["repair_succeeded"] = counter_delta(
        cold_before_runtime, cold_after_runtime, "repair_succeeded"
    )
    report["cold_search_burst"] = cold
    accepted_loads = {0, 1} if args.allow_warm_start else {1}
    add_gate(
        report,
        "cold_search_single_flight",
        cold["sidecar_loads"] in accepted_loads
        and cold["fallback_builds"] == 0
        and cold["all_results_equal"],
        f"sidecar_loads in {sorted(accepted_loads)}, fallback_builds=0, identical results",
        f"sidecar_loads={cold['sidecar_loads']}, fallback_builds={cold['fallback_builds']}, equal={cold['all_results_equal']}",
    )
    add_gate(
        report,
        "cold_search_latency",
        cold["wall_ms"] <= 10_000.0,
        "16 callers complete in <= 10000 ms",
        f"{cold['wall_ms']:.3f} ms",
    )
    cold_monitor = cold["runtime_monitor"]
    add_gate(
        report,
        "cold_event_loop_responsive",
        cold_monitor["health_samples"] > 0
        and not cold_monitor["monitor_errors"]
        and cold_monitor["health_p99_ms"] < 50.0,
        "health p99 < 50 ms with no monitor errors",
        f"p99={cold_monitor['health_p99_ms']:.3f} ms, samples={cold_monitor['health_samples']}, errors={len(cold_monitor['monitor_errors'])}",
    )

    indexes = nested(after_cold, "retrieval", "search_indexes", default=[])
    indexes = indexes if isinstance(indexes, list) else []
    largest_index = max(
        (item for item in indexes if isinstance(item, dict)),
        key=lambda item: integer(item.get("index_bytes")),
        default={},
    )
    report["observed_search_index"] = {
        "artifact_version": largest_index.get("artifact_version"),
        "documents": integer(largest_index.get("documents")),
        "index_bytes": integer(largest_index.get("index_bytes")),
        "retained": largest_index.get("retained") is True,
    }
    observed_index_bytes = integer(largest_index.get("index_bytes"))
    add_gate(
        report,
        "platform_index_size",
        0 < observed_index_bytes <= SEARCH_INDEX_LIMIT_BYTES
        and largest_index.get("retained") is True,
        f"0 < index_bytes <= {SEARCH_INDEX_LIMIT_BYTES} and retained=true",
        f"bytes={observed_index_bytes}, retained={largest_index.get('retained')}",
    )

    before_warm = http.get_json("/operations/metrics")
    warm = concurrent_tool_calls(
        client,
        "search_code",
        repo_arguments(args.repo, query=args.search_query, limit=25),
        SEARCH_CONCURRENCY,
        http,
    )
    after_warm = http.get_json("/operations/metrics")
    warm_before_runtime = search_runtime(before_warm)
    warm_after_runtime = search_runtime(after_warm)
    warm["sidecar_loads"] = counter_delta(
        warm_before_runtime, warm_after_runtime, "sidecar_loaded"
    )
    warm["fallback_builds"] = counter_delta(
        warm_before_runtime, warm_after_runtime, "fallback_builds"
    )
    warm["cache_hits"] = counter_delta(
        search_cache(before_warm), search_cache(after_warm), "hits"
    )
    report["warm_search_burst"] = warm
    add_gate(
        report,
        "warm_search_latency",
        warm["p95_ms"] <= 500.0
        and warm["sidecar_loads"] == 0
        and warm["fallback_builds"] == 0
        and warm["cache_hits"] >= SEARCH_CONCURRENCY
        and warm["all_results_equal"],
        "p95 <= 500 ms, >= 16 cache hits, no load/build, identical results",
        f"p95={warm['p95_ms']:.3f} ms, loads={warm['sidecar_loads']}, builds={warm['fallback_builds']}, hits={warm['cache_hits']}",
    )
    warm_monitor = warm["runtime_monitor"]
    add_gate(
        report,
        "warm_scorer_scratch",
        warm_monitor["max_scorer_active"] > 0
        and warm_monitor["max_scorer_scratch_bytes"] <= SCORER_TOTAL_SCRATCH_LIMIT_BYTES
        and warm_monitor["max_scratch_per_active_bytes"]
        <= SCORER_PER_ACTIVE_LIMIT_BYTES,
        "observed active scoring; aggregate <= 32 MiB and per active scorer <= 6 MiB",
        f"active={warm_monitor['max_scorer_active']}, aggregate={warm_monitor['max_scorer_scratch_bytes']}, per_active={warm_monitor['max_scratch_per_active_bytes']}",
    )
    add_gate(
        report,
        "warm_event_loop_responsive",
        warm_monitor["health_samples"] > 0
        and not warm_monitor["monitor_errors"]
        and warm_monitor["health_p99_ms"] < 50.0,
        "health p99 < 50 ms with no monitor errors",
        f"p99={warm_monitor['health_p99_ms']:.3f} ms, samples={warm_monitor['health_samples']}, errors={len(warm_monitor['monitor_errors'])}",
    )

    overview_without_wiki = one_tool_call(
        client,
        "architecture_overview",
        repo_arguments(
            args.repo,
            sections=["stats", "modules", "route_groups", "entrypoints"],
            limit=25,
        ),
    )
    report["overview_without_wiki"] = overview_without_wiki
    add_gate(
        report,
        "overview_without_wiki",
        overview_without_wiki["elapsed_ms"] <= 2_000.0,
        "<= 2000 ms",
        f"{overview_without_wiki['elapsed_ms']:.3f} ms",
    )

    overview_default = one_tool_call(
        client,
        "architecture_overview",
        repo_arguments(args.repo, sections=[], limit=25),
    )
    report["overview_default"] = overview_default
    add_gate(
        report,
        "overview_default_fail_soft",
        overview_default["elapsed_ms"] <= 2_000.0,
        "<= 2000 ms including optional wiki handling",
        f"{overview_default['elapsed_ms']:.3f} ms",
    )

    scoped_grep = one_tool_call(
        client,
        "grep_files",
        repo_arguments(
            args.repo,
            pattern=args.grep_pattern,
            glob=args.grep_glob,
            limit=50,
        ),
    )
    report["scoped_grep"] = scoped_grep
    add_gate(
        report,
        "scoped_grep_latency",
        scoped_grep["elapsed_ms"] <= 10_000.0,
        "<= 10000 ms",
        f"{scoped_grep['elapsed_ms']:.3f} ms",
    )

    no_match_grep = one_tool_call(
        client,
        "grep_files",
        repo_arguments(
            args.repo,
            pattern=args.no_match_pattern,
            glob=args.grep_glob,
            limit=50,
        ),
    )
    report["no_match_grep"] = no_match_grep
    no_match_summary = no_match_grep["result_summary"]
    complete_or_partial = isinstance(no_match_summary.get("complete"), bool)
    add_gate(
        report,
        "no_match_grep_bounded",
        no_match_grep["elapsed_ms"] <= 85_000.0 and complete_or_partial,
        "complete or explicit partial response in <= 85000 ms",
        f"elapsed={no_match_grep['elapsed_ms']:.3f} ms, complete={no_match_summary.get('complete')}, reason={no_match_summary.get('truncation_reason')}",
    )

    final_metrics = http.get_json("/operations/metrics")
    report["final_counters"] = {
        "search_cache": search_cache(final_metrics),
        "search_runtime": search_runtime(final_metrics),
        "grep": nested(final_metrics, "retrieval", "grep", default={}),
        "wiki_runtime": nested(final_metrics, "retrieval", "wiki_runtime", default={}),
    }
    report["passed"] = all(gate["passed"] for gate in report["gates"])
    return report


def write_report(path: Path, report: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    temporary.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def print_summary(report: dict[str, Any], output: Path) -> None:
    for gate in report.get("gates", []):
        status = "PASS" if gate.get("passed") else "FAIL"
        print(f"[{status}] {gate.get('name')}: {gate.get('observed')}")
    if report.get("error"):
        print(f"[FAIL] validation_error: {report['error']}")
    print(f"report: {output}")


def run_self_test() -> None:
    event = b'event: message\ndata: {"jsonrpc":"2.0",\ndata: "id":1,"result":{}}\n\n'
    parsed = parse_http_payload(event, "text/event-stream")
    assert parsed["id"] == 1
    assert percentile([4.0, 1.0, 3.0, 2.0], 0.95) == 4.0
    assert summarize_result([1, 2])["result_count"] == 2
    assert nested({"a": {"b": 3}}, "a", "b") == 3
    assert canonical_result({"b": 1, "a": 2}) == '{"a":2,"b":1}'

    class FakeHttp:
        def request(
            self,
            method: str,
            path: str,
            payload: dict[str, Any] | None = None,
            session_id: str | None = None,
        ) -> HttpResult:
            del method, path, session_id
            rpc_method = payload.get("method") if payload else None
            request_id = payload.get("id") if payload else None
            if rpc_method == "initialize":
                return HttpResult(
                    {"jsonrpc": "2.0", "id": request_id, "result": {}},
                    1,
                    {"Mcp-Session-Id": "test-session"},
                )
            if rpc_method == "notifications/initialized":
                return HttpResult(None, 0, {})
            if rpc_method == "tools/list":
                result = {"tools": [{"name": "search_code"}]}
            elif rpc_method == "tools/call":
                result = {"structuredContent": [{"node_id": "n1"}]}
            else:
                raise AssertionError(f"unexpected method {rpc_method}")
            return HttpResult(
                {"jsonrpc": "2.0", "id": request_id, "result": result},
                1,
                {},
            )

    client = McpClient(FakeHttp())  # type: ignore[arg-type]
    client.initialize()
    assert client.list_tools() == ["search_code"]
    structured, body_bytes = client.call_tool("search_code", {"query": "x"})
    assert structured == [{"node_id": "n1"}]
    assert body_bytes == 1
    print("self-test passed")


def current_revision() -> str:
    try:
        return subprocess.check_output(
            ["git", "rev-parse", "HEAD"],
            text=True,
            stderr=subprocess.DEVNULL,
        ).strip()
    except (OSError, subprocess.CalledProcessError):
        return "unknown"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--server-url",
        default=os.environ.get("CIH_ACCEPT_SERVER_URL", "http://127.0.0.1:8080"),
    )
    parser.add_argument(
        "--repo",
        default=os.environ.get("CIH_ACCEPT_REPO", ""),
        help="MCP repo selector; empty targets the primary repository",
    )
    parser.add_argument(
        "--repository-label",
        default=os.environ.get("CIH_ACCEPT_REPOSITORY_LABEL", "platform"),
    )
    parser.add_argument(
        "--artifacts-dir",
        type=Path,
        default=Path(
            os.environ.get(
                "CIH_ACCEPT_ARTIFACTS_DIR", "/workspace/platform/.cih/artifacts"
            )
        ),
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=Path(
            os.environ.get("CIH_ACCEPT_OUTPUT", "docs/perf/search-platform-474k.json")
        ),
    )
    parser.add_argument(
        "--revision",
        default=os.environ.get("CIH_ACCEPT_REVISION", current_revision()),
    )
    parser.add_argument(
        "--token-env",
        default="CIH_API_TOKEN",
        help="environment variable containing the bearer token",
    )
    parser.add_argument("--request-timeout-secs", type=float, default=95.0)
    parser.add_argument(
        "--search-query",
        default="CustomRecTransfers verify transferservice OData",
    )
    parser.add_argument("--grep-pattern", default="CustomRecTransfers")
    parser.add_argument("--grep-glob", default="**/*.java")
    parser.add_argument(
        "--no-match-pattern",
        default="CIH_PRODUCTION_ACCEPTANCE_INTENTIONAL_NO_MATCH_7F3A91C2",
    )
    parser.add_argument(
        "--allow-warm-start",
        action="store_true",
        help="accept zero sidecar loads in the first burst; production evidence should not use this",
    )
    parser.add_argument("--self-test", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.self_test:
        run_self_test()
        return 0
    report = initial_report(args)
    try:
        report = validate(args, report)
    except Exception as error:
        report["passed"] = False
        report["error"] = str(error)[:1000]
    write_report(args.output, report)
    print_summary(report, args.output)
    return 0 if report.get("passed") is True else 1


if __name__ == "__main__":
    sys.exit(main())
