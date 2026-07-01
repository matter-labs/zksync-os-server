---
name: vastai-benchmark-runner
description: Choose, rent, prepare, and benchmark Vast.ai instances. Use when Codex needs to find suitable Vast.ai offers, compare CPU/GPU/whole-machine options, create an instance with the Vast.ai CLI, wait for SSH, install system/Rust/project dependencies, clone or sync a benchmark repository, run benchmark commands, collect results, diagnose bottlenecks, or tear down the instance.
---

# Vast.ai Benchmark Runner

## Workflow

Use this sequence for benchmark runs unless the user asks for only one phase.

1. **Clarify constraints only if missing and risky**: benchmark command, repo/branch, CPU vs GPU priority, whole-machine requirement, budget ceiling, minimum duration, and whether to rent now. If the user says not to rent yet, stop after presenting ranked options.
2. **Check local prerequisites**: confirm `vastai` exists and is authenticated. Use `vastai show user` or a harmless listing command. If missing, install with `pipx install vastai` when appropriate.
3. **Find offers**: prefer verified, high-reliability, non-busy hosts. For CPU-heavy tests, rank by physical cores, CPU model, RAM, disk bandwidth/space, direct SSH availability, datacenter reliability, and $/hr. For GPU tests, include GPU model/count/VRAM.
4. **Present options before renting** unless the user explicitly authorizes creation. Say whether each option is whole-machine or a slice. Whole-machine usually requires `num_gpus == total_gpus` and no fractional CPU/RAM allocation indicators.
5. **Create the instance** with explicit image, disk, SSH, env, and on-start behavior. Prefer a CUDA/base image only when GPU tooling is needed; CPU Rust benchmarks can use a minimal Ubuntu image.
6. **Wait for readiness**: poll instance state and SSH endpoint. Then verify with `ssh`.
7. **Set up reproducibly**: install packages, Rust/toolchains, project-specific tools, clone/sync repo, checkout exact branch/commit, build once, record machine metadata.
8. **Run benchmarks**: run warmup and measured commands, capture stdout/stderr to timestamped logs, record env vars, commit SHA, instance ID, and hardware metadata.
9. **Analyze and report**: summarize offer, cost, exact commands, benchmark results, bottleneck evidence, and next experiment. Do not leave expensive instances running without reminding the user.

## Vast.ai Commands

Use the local `vastai` CLI. Vast.ai output formats vary by version; prefer raw/JSON when available and fall back to table parsing manually.

Useful commands:

```bash
vastai search offers '<query>' --raw
vastai show instances --raw
vastai create instance <offer_id> --image <image> --disk <gb> --ssh
vastai destroy instance <instance_id>
```

Good CPU-heavy search filters to start from:

```text
verified=true reliability2>=0.98 rentable=true inet_down>500 inet_up>100 cpu_cores_effective>=32 disk_space>=100
```

Add GPU predicates only when the benchmark needs GPU.

## Helper Script

Use `scripts/vastai_bench.py` for repeatable offer scoring and command generation. It does not rent unless called with `create --execute`.

Examples:

```bash
python3 ~/.codex/skills/vastai-benchmark-runner/scripts/vastai_bench.py search \
  --query 'verified=true reliability2>=0.98 rentable=true cpu_cores_effective>=32 disk_space>=100' \
  --top 8

python3 ~/.codex/skills/vastai-benchmark-runner/scripts/vastai_bench.py create-command \
  --offer-id 42196495 --image nvidia/cuda:12.4.1-devel-ubuntu22.04 --disk 200
```

Read `references/zksync-os.md` when the benchmark target is the `zksync-os-server` repository or the user mentions `parallel_injection_tps`, `parallel_blocks_tps`, Rust integration tests, Foundry, or Anvil.

## Operational Rules

- Request explicit user approval before creating, destroying, or changing paid instances.
- Never assume an instance is running; verify state and SSH endpoint.
- Prefer exact IDs in commands. Treat Vast instance IDs and offer IDs as opaque.
- Preserve benchmark logs on the remote machine and copy back only when useful.
- Record absolute dates/times for benchmark runs and instance lifecycle actions.
- For performance diagnosis, compare K sweeps under the same command, commit, instance, warmup, duration, and log level.
