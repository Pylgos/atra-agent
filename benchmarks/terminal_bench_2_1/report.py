import argparse
import csv
import json
from collections import defaultdict
from datetime import datetime
from pathlib import Path


def read_json(path: Path) -> dict:
    return json.loads(path.read_text().replace("[REDACTED]", "null"))


def duration_seconds(timing: dict | None) -> float | None:
    if not timing or not timing.get("started_at") or not timing.get("finished_at"):
        return None
    started = datetime.fromisoformat(timing["started_at"])
    finished = datetime.fromisoformat(timing["finished_at"])
    return (finished - started).total_seconds()


def reward(result: dict) -> float | None:
    rewards = (result.get("verifier_result") or {}).get("rewards")
    if not rewards:
        return None
    if "reward" in rewards:
        return float(rewards["reward"])
    if len(rewards) == 1:
        return float(next(iter(rewards.values())))
    return None


def quota_window(snapshots: list[dict]) -> dict | None:
    for snapshot in snapshots:
        if snapshot.get("limit_id") == "codex" and snapshot.get("primary"):
            primary = snapshot["primary"]
            return {
                "limit_id": "codex",
                "used_percent": primary.get("used_percent"),
                "window_minutes": primary.get("window_minutes"),
                "resets_at": primary.get("resets_at"),
            }
    return None


def atra_metrics(trial_dir: Path, metadata: dict) -> tuple[int | None, list[dict]]:
    path = trial_dir / "agent" / "atra-events.jsonl"
    if not path.exists():
        return metadata.get("model_requests"), []
    snapshots = []
    for line in path.read_text().splitlines():
        event = json.loads(line)
        if event.get("kind") == "rate_limits":
            window = quota_window(event["payload"].get("snapshots", []))
            if window is not None:
                snapshots.append(window)
    return metadata.get("model_requests"), snapshots


def codex_metrics(trial_dir: Path) -> tuple[int | None, list[dict]]:
    requests = 0
    snapshots = []
    for path in (trial_dir / "agent" / "sessions").rglob("*.jsonl"):
        for line in path.read_text().splitlines():
            try:
                event = json.loads(line.replace("[REDACTED]", "null"))
            except json.JSONDecodeError:
                continue
            payload = event.get("payload") or {}
            if event.get("type") != "event_msg" or payload.get("type") != "token_count":
                continue
            requests += 1
            window = quota_window([payload.get("rate_limits") or {}])
            if window is not None:
                snapshots.append(window)
    return (requests or None), snapshots


def load_rows(job_dirs: list[Path]) -> list[dict]:
    rows = []
    seen = set()
    for job_dir in job_dirs:
        paths = list(job_dir.glob("*/result.json"))
        if not paths:
            paths = job_dir.rglob("result.json")
        for path in paths:
            result = read_json(path)
            if "task_name" not in result or str(result.get("id")) in seen:
                continue
            seen.add(str(result.get("id")))
            agent = result["agent_info"]
            usage = result.get("agent_result") or {}
            metadata = usage.get("metadata") or {}
            input_tokens = usage.get("n_input_tokens")
            cache_tokens = usage.get("n_cache_tokens")
            if agent["name"] == "atra":
                model_requests, quota = atra_metrics(path.parent, metadata)
            else:
                model_requests, quota = codex_metrics(path.parent)
            rows.append(
                {
                    "agent": agent["name"],
                    "agent_version": agent["version"],
                    "model": (agent.get("model_info") or {}).get("name"),
                    "reasoning_effort": result["config"]["agent"]["kwargs"].get(
                        "reasoning_effort"
                    ),
                    "task": result["task_name"],
                    "reward": reward(result),
                    "input_tokens": input_tokens,
                    "cached_input_tokens": cache_tokens,
                    "uncached_input_tokens": (
                        input_tokens - cache_tokens
                        if input_tokens is not None and cache_tokens is not None
                        else None
                    ),
                    "output_tokens": usage.get("n_output_tokens"),
                    "reasoning_output_tokens": metadata.get("reasoning_output_tokens"),
                    "model_requests": model_requests,
                    "quota_start_percent": (
                        quota[0]["used_percent"] if quota else None
                    ),
                    "quota_end_percent": (quota[-1]["used_percent"] if quota else None),
                    "quota_window_minutes": (
                        quota[-1]["window_minutes"] if quota else None
                    ),
                    "quota_start_reset_at": (quota[0]["resets_at"] if quota else None),
                    "quota_end_reset_at": (quota[-1]["resets_at"] if quota else None),
                    "agent_seconds": duration_seconds(result.get("agent_execution")),
                    "completed": (
                        result.get("exception_info") is None
                        and result.get("verifier_result") is not None
                    ),
                    "exception": (result.get("exception_info") or {}).get(
                        "exception_type"
                    ),
                    "started_at": result.get("started_at"),
                    "result_path": str(path),
                }
            )
    return rows


def total(rows: list[dict], field: str) -> int | float | None:
    values = [row[field] for row in rows if row[field] is not None]
    return sum(values) if values else None


def number(value: int | float | None, decimals: int = 0) -> str:
    if value is None:
        return "-"
    if decimals:
        return f"{value:,.{decimals}f}"
    return f"{value:,.0f}"


def quota_change(rows: list[dict]) -> str:
    measured = [
        row
        for row in sorted(rows, key=lambda row: row["started_at"] or "")
        if row["quota_start_percent"] is not None
        and row["quota_end_percent"] is not None
    ]
    if not measured:
        return "—"
    first = measured[0]
    last = measured[-1]
    if (
        first["quota_window_minutes"] != last["quota_window_minutes"]
        or last["quota_end_percent"] < first["quota_start_percent"]
    ):
        return "reset/unknown"
    start = first["quota_start_percent"]
    end = last["quota_end_percent"]
    return f"{number(start, 1)} -> {number(end, 1)} (+{number(end - start, 1)} pp)"


def short_version(version: str) -> str:
    if "+dirty." in version:
        commit, dirty = version.split("+", maxsplit=1)
        return f"{commit[:8]}+{dirty}"
    if len(version) == 40 and all(
        character in "0123456789abcdef" for character in version
    ):
        return version[:12]
    return version


def table(
    title: str,
    headers: tuple[str, ...],
    rows: list[tuple[str, ...]],
    right_aligned: set[int],
) -> str:
    widths = [
        max(len(header), *(len(row[index]) for row in rows))
        for index, header in enumerate(headers)
    ]

    def render(row: tuple[str, ...]) -> str:
        cells = [
            value.rjust(widths[index])
            if index in right_aligned
            else value.ljust(widths[index])
            for index, value in enumerate(row)
        ]
        return "| " + " | ".join(cells) + " |"

    separator = tuple(
        "-" * (width - 1) + ":" if index in right_aligned else "-" * width
        for index, width in enumerate(widths)
    )
    return "\n".join(
        [f"## {title}", "", render(headers), render(separator), *map(render, rows)]
    )


def terminal_report(rows: list[dict]) -> str:
    grouped = defaultdict(list)
    for row in rows:
        grouped[
            (
                row["agent"],
                row["agent_version"],
                row["model"],
                row["reasoning_effort"],
            )
        ].append(row)

    summary_rows = []
    for (agent, version, model, effort), agent_rows in sorted(
        grouped.items(),
        key=lambda item: tuple(value or "" for value in item[0]),
    ):
        completed = [row for row in agent_rows if row["completed"]]
        latest_completed = {}
        for row in sorted(completed, key=lambda row: row["started_at"] or ""):
            latest_completed[row["task"]] = row
        rewards = [
            row["reward"]
            for row in latest_completed.values()
            if row["reward"] is not None
        ]
        passes = sum(value > 0 for value in rewards)
        mean_reward = sum(rewards) / len(rewards) if rewards else None
        agent_seconds = total(agent_rows, "agent_seconds")
        summary_rows.append(
            (
                agent,
                short_version(version),
                model or "-",
                effort or "-",
                str(len(agent_rows)),
                str(len(latest_completed)),
                str(sum(row["exception"] is not None for row in agent_rows)),
                number(mean_reward, 3),
                f"{passes}/{len(rewards)}",
                number(total(agent_rows, "model_requests")),
                number(total(agent_rows, "input_tokens")),
                number(total(agent_rows, "cached_input_tokens")),
                number(total(agent_rows, "uncached_input_tokens")),
                number(total(agent_rows, "output_tokens")),
                quota_change(agent_rows),
                f"{number(agent_seconds / 60, 1)} min"
                if agent_seconds is not None
                else "-",
            )
        )

    summary = table(
        "Summary",
        (
            "Agent",
            "Version",
            "Model",
            "Effort",
            "Attempts",
            "Completed",
            "Errors",
            "Mean",
            "Passes",
            "Requests",
            "Input",
            "Cached",
            "Uncached",
            "Output",
            "Quota observed*",
            "Time",
        ),
        summary_rows,
        set(range(4, 16)),
    )
    task_rows = []
    for row in sorted(
        rows,
        key=lambda row: (
            row["task"],
            row["agent"],
            row["started_at"] or "",
        ),
    ):
        if row["exception"] is not None:
            status = f"error:{row['exception']}"
        elif row["completed"]:
            status = (
                "pass" if row["reward"] is not None and row["reward"] > 0 else "fail"
            )
        else:
            status = "incomplete"
        task_rows.append(
            (
                row["task"],
                row["agent"],
                short_version(row["agent_version"]),
                status,
                number(row["reward"], 3),
                number(row["model_requests"]),
                number(row["input_tokens"]),
                number(row["cached_input_tokens"]),
                number(row["uncached_input_tokens"]),
                number(row["output_tokens"]),
                quota_change([row]),
                (
                    f"{number(row['agent_seconds'] / 60, 1)} min"
                    if row["agent_seconds"] is not None
                    else "-"
                ),
            )
        )
    tasks = table(
        "Tasks",
        (
            "Task",
            "Agent",
            "Version",
            "Status",
            "Reward",
            "Requests",
            "Input",
            "Cached",
            "Uncached",
            "Output",
            "Quota observed*",
            "Time",
        ),
        task_rows,
        set(range(4, 12)),
    )
    note = (
        "* Quota is the first-to-last observed Codex weekly-window usage. "
        "It excludes the first request and has 1 percentage-point resolution."
    )
    return f"{summary}\n\n{tasks}\n\n{note}"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("job_dirs", nargs="+", type=Path)
    parser.add_argument("--csv", type=Path)
    parser.add_argument("--task", action="append")
    args = parser.parse_args()

    rows = load_rows(args.job_dirs)
    if args.task:
        tasks = set(args.task)
        rows = [row for row in rows if row["task"] in tasks]
    if not rows:
        raise SystemExit("no Harbor trial result.json files found")
    if args.csv:
        args.csv.parent.mkdir(parents=True, exist_ok=True)
        with args.csv.open("w", newline="") as output:
            writer = csv.DictWriter(output, fieldnames=rows[0].keys())
            writer.writeheader()
            writer.writerows(sorted(rows, key=lambda row: (row["task"], row["agent"])))
    print(terminal_report(rows))


if __name__ == "__main__":
    main()
