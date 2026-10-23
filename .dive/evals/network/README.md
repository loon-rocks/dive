# Network drafting evals

Times how long the drafting architect takes to build each network in `goals.yaml`, and whether the built network passes its test. `network_eval` drives a real workbench with a scripted user:

1. It sends the request.
2. It answers the interview from the goal's `answers`, or with "Go with your guess." when they run out.
3. It confirms the restate and says "Yes, build it."
4. It approves every form.

Each goal reports:
- **draft**: build start to every node drafted.
- **test**: drafted to a passing network test.
- **design**: request to sketch.
- calls and tokens.

Run it from the repo root:

```bash
cargo build --example network_eval -p dive-runtime
EVAL_ROLES="chat=anthropic:claude-sonnet-5-5,drafter=anthropic:claude-haiku-5-5" \
  target/debug/examples/network_eval --workers 4 --out /tmp/network-eval/results.json
```

- `EVAL_ROLES` overrides model roles as `role=provider:model`.
- `--only id,id` picks goals.
- Each goal runs in its own process, with a scratch data directory and scratch tool and plan directories.
