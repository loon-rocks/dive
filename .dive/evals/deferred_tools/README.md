# Deferred tool loading evals

Compares two graph binaries on the same `chat` tasks over a large catalog: 94 canned exec tools across eleven fake services (`catalog.yaml`, rendered into each scratch directory as user tools), the MCP reference server, and whatever the global config layer adds. Each turn reports whether the expected tools were called (and forbidden ones weren't, and no `user__` tool was called on a no-tool task), prompt tokens including cache reads and writes, model calls, wall time and cost.

```bash
python3 .dive/evals/deferred_tools/run.py path/to/graph-baseline --out /tmp/deferred/baseline.json --workers 6
python3 .dive/evals/deferred_tools/run.py path/to/graph-candidate --out /tmp/deferred/candidate.json --workers 6
python3 .dive/evals/deferred_tools/compare.py /tmp/deferred/baseline.json /tmp/deferred/candidate.json
```

- `--model` (or `EVAL_MODEL`) picks the `default` model, `claude-sonnet-5-5` unless set.
- `--only id,id` picks tasks; `--keep` keeps the scratch directories.
- Follow-up tasks run their later turns on the same thread with `--thread`.
- Needs `npx` for the MCP reference server and `ANTHROPIC_API_KEY` or a stored key.
