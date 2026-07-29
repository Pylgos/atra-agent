#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["harbor==0.20.0"]
# ///

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from copy import deepcopy
from datetime import UTC, datetime
from pathlib import Path

import yaml


BENCHMARK_DIR = Path(__file__).resolve().parent
REPOSITORY = BENCHMARK_DIR.parents[1]
CAMPAIGNS_DIR = BENCHMARK_DIR / "jobs" / "campaigns"
AGENT_NAMES = ("atra", "codex")


def command(
    *args: str | Path,
    check: bool = True,
    capture: bool = False,
    env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(arg) for arg in args],
        cwd=REPOSITORY,
        env=env,
        check=check,
        text=True,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.PIPE if capture else None,
    )


def source_version() -> str:
    commit = command("git", "rev-parse", "HEAD", capture=True).stdout.strip()
    diff = command(
        "git",
        "diff",
        "--binary",
        "HEAD",
        "--",
        "Cargo.toml",
        "Cargo.lock",
        "crates",
        "flake.lock",
        "flake.nix",
        "tools/platform-bundle",
        "benchmarks/terminal_bench_2_1/atra_agent.py",
        capture=True,
    ).stdout
    if not diff:
        return commit
    digest = hashlib.sha256(diff.encode()).hexdigest()[:12]
    return f"{commit}+dirty.{digest}"


def agent_name(agent: dict) -> str:
    return agent.get("name") or "atra"


def load_config(mode: str, version: str) -> dict:
    path = BENCHMARK_DIR / ("pilot.yaml" if mode == "pilot" else "job.yaml")
    config = yaml.safe_load(path.read_text())
    for agent in config["agents"]:
        if agent_name(agent) == "atra":
            agent["kwargs"]["agent_version"] = version
    return config


def benchmark_spec(config: dict) -> dict:
    datasets = deepcopy(config["datasets"])
    for dataset in datasets:
        dataset.pop("task_names")
    return {
        "n_attempts": config["n_attempts"],
        "n_concurrent_trials": config["n_concurrent_trials"],
        "timeout_multiplier": config["timeout_multiplier"],
        "environment": config["environment"],
        "datasets": datasets,
    }


def campaign_spec(mode: str, config: dict) -> dict:
    return {
        "schema": 3,
        "mode": mode,
        "benchmark": benchmark_spec(config),
        "tasks": config["datasets"][0]["task_names"],
    }


def read_json(path: Path) -> dict:
    return json.loads(path.read_text().replace("[REDACTED]", "null"))


def compatible_campaigns(spec: dict, campaign_dir: Path) -> list[Path]:
    campaigns = []
    if CAMPAIGNS_DIR.exists():
        for path in CAMPAIGNS_DIR.glob("*/campaign.json"):
            try:
                candidate = read_json(path)
            except (json.JSONDecodeError, OSError):
                continue
            if candidate.get("benchmark") == spec:
                campaigns.append(path.parent)
    if campaign_dir not in campaigns:
        campaigns.append(campaign_dir)
    return campaigns


def trial_attempts(campaign_dirs: list[Path]) -> list[dict]:
    attempts = []
    for campaign_dir in campaign_dirs:
        for path in campaign_dir.glob("jobs/*/*/result.json"):
            try:
                result = read_json(path)
            except (json.JSONDecodeError, OSError):
                continue
            if "task_name" not in result or not result.get("agent_info"):
                continue
            attempts.append(result)
    return attempts


def completed(result: dict) -> bool:
    return (
        result.get("exception_info") is None
        and result.get("verifier_result") is not None
    )


def selected_agents(config: dict, selection: str) -> list[dict]:
    if selection == "both":
        return config["agents"]
    return [agent for agent in config["agents"] if agent_name(agent) == selection]


def agent_fingerprint(agent: dict) -> tuple[str, str, str, str | None]:
    name = agent_name(agent)
    kwargs = agent["kwargs"]
    version = kwargs["agent_version"] if name == "atra" else kwargs["version"]
    model = agent["model_name"].rsplit("/", maxsplit=1)[-1]
    return name, version, model, kwargs.get("reasoning_effort")


def result_fingerprint(result: dict) -> tuple[str, str, str | None, str | None]:
    info = result["agent_info"]
    kwargs = result["config"]["agent"]["kwargs"]
    return (
        info["name"],
        info["version"],
        (info.get("model_info") or {}).get("name"),
        kwargs.get("reasoning_effort"),
    )


def plan(
    config: dict,
    campaign_dirs: list[Path],
    selection: str,
    retry_errors: bool,
    rerun_completed: bool,
) -> dict:
    attempts = trial_attempts(campaign_dirs)
    tasks = config["datasets"][0]["task_names"]
    pending_by_agent = {}
    versions = {}
    held_errors = 0
    already_completed = 0
    for agent in selected_agents(config, selection):
        name = agent_name(agent)
        fingerprint = agent_fingerprint(agent)
        versions[name] = fingerprint[1]
        matching = [
            result for result in attempts if result_fingerprint(result) == fingerprint
        ]
        completed_tasks = {
            result["task_name"] for result in matching if completed(result)
        }
        error_tasks = {
            result["task_name"]
            for result in matching
            if result.get("exception_info") is not None
        } - completed_tasks
        pending = []
        for task in tasks:
            if task in completed_tasks and not rerun_completed:
                already_completed += 1
            elif task in error_tasks and not retry_errors:
                held_errors += 1
            else:
                pending.append(task)
        pending_by_agent[name] = pending
    return {
        "attempts": len(attempts),
        "completed": already_completed,
        "held_errors": held_errors,
        "pending_by_agent": pending_by_agent,
        "versions": versions,
        "will_run": sum(map(len, pending_by_agent.values())),
    }


def print_plan(run_plan: dict) -> None:
    print(f"Compatible attempts: {run_plan['attempts']}")
    print(f"Completed:         {run_plan['completed']}")
    print(f"Errors held:       {run_plan['held_errors']}")
    print(f"Will run:          {run_plan['will_run']}")
    for agent, tasks in run_plan["pending_by_agent"].items():
        print(f"  {agent}@{run_plan['versions'][agent]}: {len(tasks)}")
    print(f"Maximum new API trials: {run_plan['will_run']}")


def confirm() -> None:
    if not sys.stdin.isatty():
        raise SystemExit("confirmation requires a terminal; pass --yes to continue")
    if input("Continue? [y/N] ").strip().lower() not in {"y", "yes"}:
        raise SystemExit("cancelled")


def check_auth(agent: str) -> None:
    if agent == "atra":
        command(REPOSITORY / "result-atra/bin/atra", "codex", "status")
        return
    codex = shutil.which("codex")
    if codex is None:
        raise SystemExit("codex is not installed")
    command(codex, "login", "status")


def docker_environment() -> tuple[
    dict[str, str], tempfile.TemporaryDirectory[str] | None
]:
    env = os.environ.copy()
    if (
        command("docker", "compose", "version", check=False, capture=True).returncode
        == 0
    ):
        return env, None
    compose = shutil.which("docker-compose")
    if compose is None:
        raise SystemExit("Docker Compose plugin or docker-compose is required")
    docker_config = tempfile.TemporaryDirectory(prefix="atra-benchmark-docker-")
    plugins = Path(docker_config.name) / "cli-plugins"
    plugins.mkdir()
    (plugins / "docker-compose").symlink_to(compose)
    env["DOCKER_CONFIG"] = docker_config.name
    return env, docker_config


def preflight(
    agents: list[str],
) -> tuple[dict[str, str], tempfile.TemporaryDirectory[str] | None]:
    if shutil.which("docker") is None:
        raise SystemExit("docker is not installed")
    if command("docker", "info", check=False, capture=True).returncode != 0:
        raise SystemExit("Docker daemon is unavailable")
    docker_env, docker_config = docker_environment()
    if "atra" in agents:
        command("nix", "build", ".#atra", "--out-link", "result-atra")
    for agent in agents:
        check_auth(agent)
    if shutil.which("harbor") is None:
        raise SystemExit("Harbor is unavailable in the uv environment")
    return docker_env, docker_config


def save_campaign(campaign_dir: Path, spec: dict, dry_run: bool) -> None:
    path = campaign_dir / "campaign.json"
    if path.exists():
        if read_json(path) != spec:
            raise SystemExit(
                f"campaign configuration changed: {path}\n"
                "Use a new --campaign name to keep comparisons reproducible."
            )
        return
    if dry_run:
        return
    campaign_dir.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(spec, indent=2) + "\n")


def run_batch(
    config: dict,
    campaign_dir: Path,
    agent: str,
    tasks: list[str],
    env: dict[str, str],
) -> None:
    timestamp = datetime.now(UTC).strftime("%Y%m%dT%H%M%S%fZ")
    job_name = f"batch-{agent}-{timestamp}"
    batch = deepcopy(config)
    batch["job_name"] = job_name
    batch["jobs_dir"] = str(campaign_dir / "jobs")
    batch["agents"] = [
        candidate for candidate in config["agents"] if agent_name(candidate) == agent
    ]
    batch["datasets"][0]["task_names"] = tasks
    configs_dir = campaign_dir / "batch-configs"
    configs_dir.mkdir(parents=True, exist_ok=True)
    config_path = configs_dir / f"{job_name}.json"
    config_path.write_text(json.dumps(batch, indent=2) + "\n")
    run_env = env.copy()
    run_env["PYTHONPATH"] = str(REPOSITORY)
    command("harbor", "run", "-c", config_path, "--yes", env=run_env)


def report(
    campaign_dirs: list[Path],
    campaign_dir: Path,
    tasks: list[str],
    write_csv: bool,
) -> None:
    result_paths = (
        path
        for campaign in campaign_dirs
        for path in campaign.glob("jobs/*/*/result.json")
    )
    if next(result_paths, None) is None:
        return
    args = [
        sys.executable,
        BENCHMARK_DIR / "report.py",
        *campaign_dirs,
        *(argument for task in tasks for argument in ("--task", task)),
    ]
    if write_csv:
        args.extend(("--csv", campaign_dir / "results.csv"))
    command(*args)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=("pilot", "full"))
    parser.add_argument("--campaign")
    parser.add_argument("--agent", choices=(*AGENT_NAMES, "both"), default="both")
    parser.add_argument("--retry-errors", action="store_true")
    parser.add_argument("--rerun-completed", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--yes", action="store_true")
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    version = source_version()
    config = load_config(args.mode, version)
    campaign = args.campaign or f"terminal-bench-2-1-{args.mode}"
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", campaign):
        raise SystemExit("campaign must contain only letters, numbers, '.', '_' or '-'")
    campaign_dir = CAMPAIGNS_DIR / campaign
    save_campaign(campaign_dir, campaign_spec(args.mode, config), args.dry_run)
    campaign_dirs = compatible_campaigns(benchmark_spec(config), campaign_dir)
    run_plan = plan(
        config,
        campaign_dirs,
        args.agent,
        args.retry_errors,
        args.rerun_completed,
    )
    print(f"Campaign: {campaign}")
    print(f"Atra version: {version}")
    print_plan(run_plan)
    sys.stdout.flush()
    if run_plan["will_run"] == 0 or args.dry_run:
        report(
            campaign_dirs,
            campaign_dir,
            config["datasets"][0]["task_names"],
            not args.dry_run,
        )
        return
    if not args.yes:
        confirm()
    agents = [agent for agent, tasks in run_plan["pending_by_agent"].items() if tasks]
    docker_env, docker_config = preflight(agents)
    try:
        for agent in agents:
            run_batch(
                config,
                campaign_dir,
                agent,
                run_plan["pending_by_agent"][agent],
                docker_env,
            )
    finally:
        if docker_config is not None:
            docker_config.cleanup()
    report(
        campaign_dirs,
        campaign_dir,
        config["datasets"][0]["task_names"],
        True,
    )


if __name__ == "__main__":
    main()
