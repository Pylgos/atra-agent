import asyncio
import json
import os
import re
import tempfile
from pathlib import Path
from typing import override

from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext


class AtraAgent(BaseAgent):
    def __init__(
        self,
        *args,
        atra_binary: str = "target/debug/atra",
        runner_binary: str | None = None,
        reasoning_effort: str = "medium",
        agent_version: str = "workspace",
        **kwargs,
    ):
        super().__init__(*args, **kwargs)
        self._atra_binary = Path(atra_binary).resolve()
        self._runner_binary = (
            Path(runner_binary).resolve() if runner_binary is not None else None
        )
        self._reasoning_effort = reasoning_effort
        self._agent_version = agent_version
        self._container_id: str | None = None
        self._remote_runner: str | None = None

    @staticmethod
    @override
    def name() -> str:
        return "atra"

    @override
    def version(self) -> str:
        return self._agent_version

    async def _command(
        self,
        *command: str,
        cwd: Path | None = None,
        env: dict[str, str] | None = None,
    ) -> tuple[int, str, str]:
        process = await asyncio.create_subprocess_exec(
            *command,
            cwd=cwd,
            env=env,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, stderr = await process.communicate()
        return (
            process.returncode,
            stdout.decode(errors="replace"),
            stderr.decode(errors="replace"),
        )

    async def _checked(
        self,
        *command: str,
        cwd: Path | None = None,
        env: dict[str, str] | None = None,
    ) -> str:
        return_code, stdout, stderr = await self._command(*command, cwd=cwd, env=env)
        if return_code != 0:
            detail = stderr.strip() or stdout.strip()
            raise RuntimeError(
                f"{command[0]} exited with {return_code}"
                + (f": {detail}" if detail else "")
            )
        return stdout

    @staticmethod
    def _compose_project_name(session_id: str) -> str:
        name = session_id.lower()
        if not re.match(r"^[a-z0-9]", name):
            name = f"0{name}"
        return re.sub(r"[^a-z0-9_-]", "-", name)

    def _docker_exec(self, environment: BaseEnvironment) -> list[str]:
        if self._container_id is None:
            raise RuntimeError("Atra agent setup has not resolved the task container")

        command = ["docker", "exec", "-i"]
        if environment.default_user is not None:
            command.extend(["--user", str(environment.default_user)])
        workdir = environment.task_env_config.workdir
        if workdir is not None:
            command.extend(["--workdir", workdir])
        command.append(self._container_id)
        return command

    @override
    async def setup(self, environment: BaseEnvironment) -> None:
        if not self._atra_binary.is_file():
            raise FileNotFoundError(f"Atra binary not found: {self._atra_binary}")
        if self._runner_binary is not None and not self._runner_binary.is_file():
            raise FileNotFoundError(
                f"Atra Runner binary not found: {self._runner_binary}"
            )

        project = self._compose_project_name(environment.session_id)
        container_ids = (
            await self._checked(
                "docker",
                "ps",
                "--filter",
                f"label=com.docker.compose.project={project}",
                "--filter",
                "label=com.docker.compose.service=main",
                "--format",
                "{{.ID}}",
            )
        ).split()
        if len(container_ids) != 1:
            raise RuntimeError(
                f"expected one Harbor main container for {project}, "
                f"found {len(container_ids)}"
            )
        self._container_id = container_ids[0]

        upload = [str(self._atra_binary), "runner", "upload"]
        if self._runner_binary is not None:
            upload.extend(["--runner-binary", str(self._runner_binary)])
        upload.extend(["--", *self._docker_exec(environment), "/bin/sh"])
        self._remote_runner = (await self._checked(*upload)).strip()

    @staticmethod
    def _usage(events: str) -> tuple[dict[str, int], int]:
        totals = {
            "input_tokens": 0,
            "cached_input_tokens": 0,
            "cache_write_input_tokens": 0,
            "output_tokens": 0,
            "reasoning_output_tokens": 0,
            "total_tokens": 0,
        }
        requests = 0
        for line in events.splitlines():
            event = json.loads(line)
            if event.get("kind") != "token_usage":
                continue
            usage = event["payload"]["usage"]
            requests += 1
            for field in totals:
                totals[field] += int(usage.get(field, 0))
        return totals, requests

    @override
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        if self._remote_runner is None:
            raise RuntimeError("Atra agent setup did not upload a Runner")
        if not self.model_name:
            raise ValueError("model_name is required")

        self.logs_dir.mkdir(parents=True, exist_ok=True)
        runtime = tempfile.TemporaryDirectory(prefix="atra-harbor-")
        workspace = Path(runtime.name) / "workspace"
        workspace.mkdir(exist_ok=True)
        state = Path(runtime.name) / "state.sqlite"
        output_path = self.logs_dir / "atra-output.txt"
        events_path = self.logs_dir / "atra-events.jsonl"
        controller_log_path = self.logs_dir / "atra-controller.log"
        model = self.model_name.rsplit("/", maxsplit=1)[-1]

        endpoint = Path(runtime.name) / "controller.sock"
        controller_env = os.environ.copy()
        controller_env["ATRA_CONTROLLER_ENDPOINT"] = str(endpoint)
        controller_env["ATRA_CONTROLLER_STATE"] = str(state)

        with controller_log_path.open("wb") as controller_log:
            controller = await asyncio.create_subprocess_exec(
                str(self._atra_binary),
                "controller",
                "run",
                cwd=workspace,
                env=controller_env,
                stdout=controller_log,
                stderr=controller_log,
            )
            thread_id: str | None = None
            try:
                for _ in range(100):
                    if controller.returncode is not None:
                        raise RuntimeError(
                            f"Atra Controller exited with {controller.returncode}"
                        )
                    status, output, _ = await self._command(
                        str(self._atra_binary),
                        "controller",
                        "status",
                        cwd=workspace,
                        env=controller_env,
                    )
                    if status == 0 and output.strip() == "running":
                        break
                    await asyncio.sleep(0.1)
                else:
                    raise RuntimeError("Atra Controller did not become ready")

                await self._checked(
                    str(self._atra_binary),
                    "runner",
                    "launch",
                    "--name",
                    "task",
                    "--description",
                    "Run all commands in the Terminal-Bench task container",
                    "--approval",
                    "allow",
                    "--",
                    *self._docker_exec(environment),
                    self._remote_runner,
                    "--stdio",
                    cwd=workspace,
                    env=controller_env,
                )
                thread_id = (
                    await self._checked(
                        str(self._atra_binary),
                        "thread",
                        "create",
                        "--name",
                        environment.session_id,
                        cwd=workspace,
                        env=controller_env,
                    )
                ).strip()
                await self._checked(
                    str(self._atra_binary),
                    "thread",
                    "model",
                    "--thread",
                    thread_id,
                    "--model",
                    model,
                    "--reasoning-effort",
                    self._reasoning_effort,
                    cwd=workspace,
                    env=controller_env,
                )
                return_code, stdout, stderr = await self._command(
                    str(self._atra_binary),
                    "thread",
                    "send",
                    "--thread",
                    thread_id,
                    "--message",
                    instruction,
                    cwd=workspace,
                    env=controller_env,
                )
                output_path.write_text(stdout + stderr)
                if return_code != 0:
                    raise RuntimeError(
                        f"Atra turn exited with {return_code}: "
                        f"{(stderr.strip() or stdout.strip())}"
                    )
            finally:
                if thread_id is not None:
                    _, events, events_error = await self._command(
                        str(self._atra_binary),
                        "thread",
                        "events",
                        "--thread",
                        thread_id,
                        cwd=workspace,
                        env=controller_env,
                    )
                    events_path.write_text(events)
                    if events:
                        usage, requests = self._usage(events)
                        context.n_input_tokens = usage["input_tokens"]
                        context.n_cache_tokens = usage["cached_input_tokens"]
                        context.n_output_tokens = usage["output_tokens"]
                        context.metadata = {
                            "model_requests": requests,
                            "reasoning_effort": self._reasoning_effort,
                            "reasoning_output_tokens": usage["reasoning_output_tokens"],
                            "cache_write_input_tokens": usage[
                                "cache_write_input_tokens"
                            ],
                            "total_tokens": usage["total_tokens"],
                            "events_error": events_error.strip() or None,
                        }

                await self._command(
                    str(self._atra_binary),
                    "controller",
                    "stop",
                    cwd=workspace,
                    env=controller_env,
                )
                try:
                    await asyncio.wait_for(controller.wait(), timeout=5)
                except TimeoutError:
                    controller.terminate()
                    await controller.wait()
                runtime.cleanup()
