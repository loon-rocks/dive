import argparse
import concurrent.futures
import json
import os
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

import yaml

HERE = Path(__file__).resolve().parent


def tool_yaml(service, name, spec):
    params = spec.get("params", {})
    return {
        "version": 3,
        "name": f"{service}_{name}",
        "description": spec["description"],
        "kind": "exec",
        "command": "printf",
        "args": ["%s", json.dumps(spec["returns"])],
        "output": "json",
        "timeout_secs": 10,
        "read_only": True,
        "input_schema": {
            "type": "object",
            "required": list(params),
            "properties": {
                key: {"type": "string", "description": text}
                for key, text in params.items()
            },
        },
    }


def scratch(root, model):
    tools = root / ".graph" / "tools"
    tools.mkdir(parents=True)
    catalog = yaml.safe_load((HERE / "catalog.yaml").read_text())
    for service, entries in catalog.items():
        for name, spec in entries.items():
            path = tools / f"{service}_{name}.yaml"
            path.write_text(yaml.safe_dump(tool_yaml(service, name, spec), sort_keys=False))
    (root / ".graph" / "config.toml").write_text(
        f"""version = 5

[settings]
data_dir = "{root / 'data'}"

[models.default]
provider = "anthropic"
model = "{model}"

[mcp.everything]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-everything"]
"""
    )


def run_turn(binary, cwd, message, thread, env):
    args = [binary, "ask", "--json", message]
    if thread:
        args += ["--thread", thread]
    started = time.monotonic()
    proc = subprocess.run(args, cwd=cwd, env=env, capture_output=True, text=True, timeout=600)
    elapsed = time.monotonic() - started
    try:
        envelope = json.loads(proc.stdout)
    except json.JSONDecodeError:
        envelope = {}
    return {
        "exit": proc.returncode,
        "seconds": round(elapsed, 2),
        "envelope": envelope,
        "stderr_tail": proc.stderr[-2000:] if proc.returncode else "",
    }


def run_task(binary, task, model, keep):
    root = Path(tempfile.mkdtemp(prefix=f"deferred-{task['id']}-"))
    try:
        scratch(root, model)
        env = dict(os.environ)
        thread = None
        turns = []
        for turn in task["turns"]:
            result = run_turn(binary, root, turn["message"], thread, env)
            thread = thread or result["envelope"].get("thread_id")
            used = result["envelope"].get("tools_used") or []
            result["expect"] = turn.get("expect", [])
            result["forbid"] = turn.get("forbid", [])
            result["tools_used"] = used
            result["hit"] = all(tool in used for tool in result["expect"]) and not any(
                tool in used for tool in result["forbid"]
            )
            if not result["expect"]:
                result["hit"] = result["hit"] and not [
                    tool for tool in used if tool.startswith("user__")
                ]
            turns.append(result)
        return {"id": task["id"], "kind": task["kind"], "turns": turns}
    finally:
        if not keep:
            shutil.rmtree(root, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary")
    parser.add_argument("--out", required=True)
    parser.add_argument("--model", default=os.environ.get("EVAL_MODEL", "claude-sonnet-5-5"))
    parser.add_argument("--only", default="")
    parser.add_argument("--workers", type=int, default=4)
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()
    tasks = yaml.safe_load((HERE / "tasks.yaml").read_text())
    if args.only:
        wanted = set(args.only.split(","))
        tasks = [task for task in tasks if task["id"] in wanted]
    binary = str(Path(args.binary).resolve())
    with concurrent.futures.ThreadPoolExecutor(args.workers) as pool:
        results = list(pool.map(lambda task: run_task(binary, task, args.model, args.keep), tasks))
    Path(args.out).write_text(
        json.dumps({"binary": binary, "model": args.model, "tasks": results}, indent=2)
    )


if __name__ == "__main__":
    main()
