import argparse
import json

PRICES = {
    "claude-sonnet-5-5": (2.00, 10.00),
    "claude-opus-5-5": (4.00, 20.00),
    "claude-haiku-5-5": (0.10, 0.50),
}


def cost(by_model):
    total = 0.0
    for row in by_model:
        price = PRICES.get(row.get("model"))
        if not price:
            continue
        inp, out = price
        total += (
            row.get("input_tokens", 0) * inp
            + row.get("cache_creation_input_tokens", 0) * inp * 1.25
            + row.get("cache_read_input_tokens", 0) * inp * 0.1
            + row.get("output_tokens", 0) * out
        ) / 1_000_000
    return total


def summarize(path):
    data = json.load(open(path))
    rows = {}
    for task in data["tasks"]:
        for index, turn in enumerate(task["turns"]):
            usage = turn["envelope"].get("usage", {})
            by_model = usage.get("by_model", [])
            rows[(task["id"], index)] = {
                "kind": task["kind"],
                "ok": turn["exit"] == 0,
                "hit": turn["hit"],
                "prompt": usage.get("input_tokens", 0)
                + usage.get("cache_creation_input_tokens", 0)
                + usage.get("cache_read_input_tokens", 0),
                "write": usage.get("cache_creation_input_tokens", 0),
                "output": usage.get("output_tokens", 0),
                "calls": usage.get("calls", 0),
                "seconds": turn["seconds"],
                "cost": cost(by_model),
                "tools": turn["tools_used"],
            }
    return data, rows


def totals(rows):
    count = len(rows) or 1
    return {
        "turns": len(rows),
        "failed": sum(not row["ok"] for row in rows.values()),
        "hit_rate": sum(row["hit"] for row in rows.values()) / count,
        "prompt_per_turn": sum(row["prompt"] for row in rows.values()) / count,
        "cache_write_per_turn": sum(row["write"] for row in rows.values()) / count,
        "calls_per_turn": sum(row["calls"] for row in rows.values()) / count,
        "seconds_per_turn": sum(row["seconds"] for row in rows.values()) / count,
        "cost": sum(row["cost"] for row in rows.values()),
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("baseline")
    parser.add_argument("candidate")
    args = parser.parse_args()
    _, base = summarize(args.baseline)
    _, cand = summarize(args.candidate)
    print(f"{'task':<28}{'hit':>9}{'prompt tok':>22}{'calls':>9}{'sec':>13}{'cost $':>17}")
    for key in sorted(base.keys() | cand.keys()):
        b, c = base.get(key), cand.get(key)
        if not b or not c:
            continue
        name = f"{key[0]}#{key[1]}"
        print(
            f"{name:<28}{int(b['hit'])}->{int(c['hit']):<6}"
            f"{b['prompt']:>10}->{c['prompt']:<10}"
            f"{b['calls']:>4}->{c['calls']:<3}"
            f"{b['seconds']:>6.1f}->{c['seconds']:<5.1f}"
            f"{b['cost']:>7.4f}->{c['cost']:<7.4f}"
        )
        if b["tools"] != c["tools"]:
            print(f"{'':<28}tools {b['tools']} -> {c['tools']}")
    tb, tc = totals(base), totals(cand)
    print()
    for key in tb:
        print(f"{key:<22}{tb[key]:>14.4f}{tc[key]:>14.4f}")
    by_kind = sorted({row["kind"] for row in base.values()})
    print()
    for kind in by_kind:
        kb = totals({k: v for k, v in base.items() if v["kind"] == kind})
        kc = totals({k: v for k, v in cand.items() if v["kind"] == kind})
        print(
            f"{kind:<12} hit {kb['hit_rate']:.2f}->{kc['hit_rate']:.2f}  "
            f"prompt/turn {kb['prompt_per_turn']:.0f}->{kc['prompt_per_turn']:.0f}  "
            f"calls/turn {kb['calls_per_turn']:.2f}->{kc['calls_per_turn']:.2f}  "
            f"cost {kb['cost']:.4f}->{kc['cost']:.4f}"
        )


if __name__ == "__main__":
    main()
