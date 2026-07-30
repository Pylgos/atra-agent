#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["harbor==0.20.0"]
# ///

import argparse
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
from copy import deepcopy
from datetime import UTC, datetime
from pathlib import Path

import yaml


BENCHMARK_DIR = Path(__file__).resolve().parent
REPOSITORY = BENCHMARK_DIR.parents[1]
JOBS_DIR = BENCHMARK_DIR / "jobs"
CAMPAIGNS_DIR = BENCHMARK_DIR / "campaigns"
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


def agent_name(agent: dict) -> str:
    return agent.get("name") or "atra"


def load_config(mode: str) -> dict:
    path = BENCHMARK_DIR / ("pilot.yaml" if mode == "pilot" else "job.yaml")
    return yaml.safe_load(path.read_text())


def selected_agent(config: dict, name: str) -> dict:
    return next(agent for agent in config["agents"] if agent_name(agent) == name)


def campaign_dir(name: str) -> Path:
    return CAMPAIGNS_DIR / name


def campaign_spec(agent: str) -> dict:
    return {"schema": 1, "agent": agent}


def check_campaign(path: Path, agent: str) -> None:
    manifest = path / "campaign.json"
    if not manifest.exists():
        return
    if read_json(manifest) != campaign_spec(agent):
        raise SystemExit(
            f"campaign belongs to another agent: {manifest}\n"
            "Use a different --campaign name."
        )


def create_campaign(path: Path, agent: str) -> None:
    path.mkdir(parents=True, exist_ok=True)
    manifest = path / "campaign.json"
    if not manifest.exists():
        manifest.write_text(json.dumps(campaign_spec(agent), indent=2) + "\n")


def read_json(path: Path) -> dict:
    return json.loads(path.read_text().replace("[REDACTED]", "null"))


def completed(result: dict) -> bool:
    return (
        result.get("exception_info") is None
        and result.get("verifier_result") is not None
    )


def task_key(task: str) -> str:
    return task.replace("/", "__")


def selected_result(path: Path, task: str, agent: str) -> dict | None:
    link = path / "results" / task_key(task)
    if link.is_symlink() and not link.exists():
        raise SystemExit(f"selected result link is broken: {link}")
    if link.exists() and not link.is_symlink():
        raise SystemExit(f"selected result is not a symlink: {link}")
    result_path = link / "result.json"
    if not result_path.exists():
        return None
    try:
        result = read_json(result_path)
    except (json.JSONDecodeError, OSError) as error:
        raise SystemExit(f"failed to read selected result: {result_path}: {error}")
    if (
        result.get("task_name") != task
        or (result.get("agent_info") or {}).get("name") != agent
    ):
        raise SystemExit(f"selected result does not match its campaign: {result_path}")
    return result


def plan(
    config: dict,
    path: Path,
    agent: str,
    retry_errors: bool,
    rerun_completed: bool,
) -> dict:
    tasks = config["datasets"][0]["task_names"]
    pending = []
    selected = 0
    held_errors = 0
    for task in tasks:
        result = selected_result(path, task, agent)
        if result is None:
            pending.append(task)
        elif completed(result) and not rerun_completed:
            selected += 1
        elif result.get("exception_info") is not None and not retry_errors:
            held_errors += 1
        else:
            pending.append(task)
    return {
        "selected": selected,
        "held_errors": held_errors,
        "pending": pending,
    }


def print_plan(run_plan: dict) -> None:
    print(f"Selected results:  {run_plan['selected']}")
    print(f"Errors held:       {run_plan['held_errors']}")
    print(f"Will run:          {len(run_plan['pending'])}")
    print(f"Maximum new API trials: {len(run_plan['pending'])}")


def confirm() -> None:
    if not sys.stdin.isatty():
        raise SystemExit("confirmation requires a terminal; pass --yes to continue")
    if input("Continue? [y/N] ").strip().lower() not in {"y", "yes"}:
        raise SystemExit("cancelled")


def check_codex_auth() -> None:
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
    agent: str,
) -> tuple[dict[str, str], tempfile.TemporaryDirectory[str] | None]:
    if shutil.which("docker") is None:
        raise SystemExit("docker is not installed")
    if command("docker", "info", check=False, capture=True).returncode != 0:
        raise SystemExit("Docker daemon is unavailable")
    docker_env, docker_config = docker_environment()
    if agent == "atra":
        command("nix", "build", ".#atra", "--out-link", "result-atra")
    else:
        check_codex_auth()
    if shutil.which("harbor") is None:
        raise SystemExit("Harbor is unavailable in the uv environment")
    return docker_env, docker_config


def link_result(path: Path, trial_dir: Path, task: str) -> None:
    results_dir = path / "results"
    results_dir.mkdir(parents=True, exist_ok=True)
    link = results_dir / task_key(task)
    temporary = results_dir / f".{link.name}.tmp"
    temporary.unlink(missing_ok=True)
    relative_target = os.path.relpath(trial_dir, results_dir)
    temporary.symlink_to(relative_target, target_is_directory=True)
    os.replace(temporary, link)


def collect_results(path: Path, job_dir: Path, agent: str) -> None:
    for result_path in job_dir.glob("*/result.json"):
        try:
            result = read_json(result_path)
        except (json.JSONDecodeError, OSError):
            continue
        if (
            result.get("task_name")
            and (result.get("agent_info") or {}).get("name") == agent
        ):
            link_result(path, result_path.parent, result["task_name"])


def run_job(
    config: dict,
    path: Path,
    agent: str,
    tasks: list[str],
    env: dict[str, str],
) -> None:
    job_name = datetime.now(UTC).strftime(f"%Y%m%dT%H%M%S%fZ-{agent}")
    batch = deepcopy(config)
    batch["job_name"] = job_name
    batch["jobs_dir"] = str(JOBS_DIR)
    batch["agents"] = [selected_agent(config, agent)]
    batch["datasets"][0]["task_names"] = tasks
    JOBS_DIR.mkdir(parents=True, exist_ok=True)
    job_dir = JOBS_DIR / job_name
    with tempfile.TemporaryDirectory(prefix="atra-benchmark-config-") as directory:
        config_path = Path(directory) / "job.json"
        config_path.write_text(json.dumps(batch, indent=2) + "\n")
        run_env = env.copy()
        run_env["PYTHONPATH"] = str(REPOSITORY)
        process = subprocess.Popen(
            ["harbor", "run", "-c", str(config_path), "--yes"],
            cwd=REPOSITORY,
            env=run_env,
            text=True,
        )
        try:
            try:
                returncode = process.wait()
            except KeyboardInterrupt:
                previous_handler = signal.signal(signal.SIGINT, signal.SIG_IGN)
                try:
                    process.wait()
                finally:
                    signal.signal(signal.SIGINT, previous_handler)
                raise
        finally:
            collect_results(path, job_dir, agent)
        if returncode:
            raise subprocess.CalledProcessError(returncode, process.args)


def report(path: Path, tasks: list[str], write_csv: bool) -> None:
    results_dir = path / "results"
    if not any(results_dir.glob("*/result.json")):
        return
    args = [
        sys.executable,
        BENCHMARK_DIR / "report.py",
        results_dir,
        *(argument for task in tasks for argument in ("--task", task)),
    ]
    if write_csv:
        args.extend(("--csv", path / "results.csv"))
    command(*args)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=("pilot", "full"))
    parser.add_argument("--campaign", required=True)
    parser.add_argument("--agent", choices=AGENT_NAMES, required=True)
    parser.add_argument("--retry-errors", action="store_true")
    parser.add_argument("--rerun-completed", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--yes", action="store_true")
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", args.campaign):
        raise SystemExit("campaign must contain only letters, numbers, '.', '_' or '-'")
    config = load_config(args.mode)
    path = campaign_dir(args.campaign)
    check_campaign(path, args.agent)
    run_plan = plan(
        config,
        path,
        args.agent,
        args.retry_errors,
        args.rerun_completed,
    )
    print(f"Campaign: {args.campaign}")
    print(f"Agent:    {args.agent}")
    print_plan(run_plan)
    sys.stdout.flush()
    if not run_plan["pending"] or args.dry_run:
        report(path, config["datasets"][0]["task_names"], not args.dry_run)
        return
    if not args.yes:
        confirm()
    docker_env, docker_config = preflight(args.agent)
    try:
        create_campaign(path, args.agent)
        run_job(
            config,
            path,
            args.agent,
            run_plan["pending"],
            docker_env,
        )
    finally:
        if docker_config is not None:
            docker_config.cleanup()
    report(path, config["datasets"][0]["task_names"], True)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        raise SystemExit(130) from None
