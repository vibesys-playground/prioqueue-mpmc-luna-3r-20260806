# prioqueue-mpmc-luna-3r-20260806

VibeSys 3-round MPMC priority queue run, gpt-5.6-luna (default effort), 2026-08-06.

VibeSys optimized a bounded priority queue (copying C ABI, Rust seed in
`workspace/src/lib.rs`) for the **MPMC** scenario (4 producers / 4 consumers).
Headline metric: `total_ops_per_sec` from the trusted evaluator (maximize).
Correctness (linearizability, priority order, no fabrication/duplication,
capacity) is a hard gate.

- Agent: Codex CLI, model `gpt-5.6-luna`, reasoning effort: default
- Rounds: 3
- Hardware: single host, see `logs/gpu.json`

## Rounds

| Round | Commit | Correctness | total_ops_per_sec |
| --- | --- | --- | --- |
| 1 | `192ac3d` (tag `round-1`) | pass | 727,512 |
| 2 | `65996d5` (tag `round-2`) | pass | 581,311 |
| 3 | `4def29b` (tag `round-3`) | pass | 799,848 |

## Layout

- `workspace/`: candidate workspace. One commit per agent step
  (`round-N-retry-M-{implementer,judge,framework-accuracy,framework-benchmark}`).
  `workspace/progress.md` and `workspace/roadmap.md` hold the agents' notes;
  `workspace/_evaluator/priority-queue/` is the trusted evaluator.
- `logs/run-*.log`: full orchestrator / implementer / judge transcripts.
- `logs/rounds.json`: per-round commit, gate result, and metric.
- `logs/usage.jsonl`: per-agent-call token usage and duration.
