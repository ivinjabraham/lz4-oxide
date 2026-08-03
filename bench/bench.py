#!/usr/bin/env python3

import argparse
import hashlib
import json
import os
import platform
import re
import shlex
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from datetime import date
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BENCH_DIR = ROOT / "bench"
UPSTREAM = ROOT / "upstream"
TESTS = UPSTREAM / "tests"
PROGRAMS = UPSTREAM / "programs"
STATIC_LIB = ROOT / "target" / "release" / "liblz4_rs.a"
DEFAULT_OUTPUT = BENCH_DIR / "results.json"
DEFAULT_WORK = Path(os.environ.get("LZ4_BENCH_WORK", Path(tempfile.gettempdir()) / "lz4-oxide-bench"))
FULLBENCH_FLAGS = [
    "-O3",
    "-g",
    "-DNDEBUG",
    "-DLZ4_DEBUG=0",
    "-DXXH_NAMESPACE=LZ4_",
    f"-I{UPSTREAM / 'lib'}",
    f"-I{PROGRAMS}",
]
RUST_LINK_LIBS = ["-lgcc_s", "-lutil", "-lrt", "-lpthread", "-lm", "-ldl", "-lc"]
SPEED_PATTERN = re.compile(r"([0-9]+(?:\.[0-9]+)?) MB/s")
DIFF_PATTERN = re.compile(r"byte-identical:\s*(\d+)\s+diverged:\s*(\d+)")


def log(message: str) -> None:
    print(message, flush=True)


def command_text(command: list[str]) -> str:
    return shlex.join(str(part) for part in command)


def run(command: list[str], *, cwd: Path = ROOT, timeout: int = 600, quiet: bool = False) -> None:
    if not quiet:
        log(f"$ {command_text(command)}")
    subprocess.run(command, cwd=cwd, check=True, timeout=timeout)


def capture(command: list[str], *, cwd: Path = ROOT, timeout: int = 600, quiet: bool = False) -> str:
    if not quiet:
        log(f"$ {command_text(command)}")
    completed = subprocess.run(
        command,
        cwd=cwd,
        check=False,
        timeout=timeout,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if completed.returncode != 0:
        sys.stderr.write(completed.stdout)
        raise subprocess.CalledProcessError(completed.returncode, command)
    return completed.stdout


def require_tools() -> None:
    if sys.platform != "linux":
        raise RuntimeError("bench/bench.py currently supports Linux only")
    missing = [tool for tool in ("cargo", "gcc", "git", "make", "strip") if shutil.which(tool) is None]
    if missing:
        raise RuntimeError(f"missing required tools: {', '.join(missing)}")


def verify_upstream() -> None:
    tree_entry = capture(["git", "ls-tree", "HEAD", "upstream"], quiet=True).split()
    if len(tree_entry) < 3:
        raise RuntimeError("cannot read the pinned upstream gitlink from HEAD")
    expected_commit = tree_entry[2]
    current_commit = capture(["git", "-C", str(UPSTREAM), "rev-parse", "HEAD"], quiet=True).strip()
    upstream_status = capture(["git", "-C", str(UPSTREAM), "status", "--short"], quiet=True).strip()
    if current_commit != expected_commit:
        raise RuntimeError(f"upstream is at {current_commit}, expected {expected_commit}")
    if upstream_status:
        raise RuntimeError(f"upstream working tree is dirty:\n{upstream_status}")
    run(["make", "kickoff-verify"])


def remove_generated_builds() -> None:
    for cache in (TESTS / "cachedObjs", PROGRAMS / "cachedObjs"):
        shutil.rmtree(cache, ignore_errors=True)
    for name in ("datagen", "fullbench"):
        path = TESTS / name
        if path.is_symlink():
            path.unlink()
    for name in ("lz4", "lz4c", "lz4cat", "unlz4"):
        path = PROGRAMS / name
        if path.is_symlink():
            path.unlink()


def copy_resolved(source: Path, destination: Path) -> None:
    shutil.copy2(source.resolve(strict=True), destination)


def compile_fullbench_c(destination: Path) -> None:
    command = [
        "gcc",
        *FULLBENCH_FLAGS,
        str(TESTS / "fullbench.c"),
        str(UPSTREAM / "lib" / "lz4.c"),
        str(UPSTREAM / "lib" / "lz4hc.c"),
        str(UPSTREAM / "lib" / "lz4frame.c"),
        str(UPSTREAM / "lib" / "xxhash.c"),
        "-o",
        str(destination),
    ]
    run(command)


def compile_fullbench_rust(destination: Path) -> None:
    command = [
        "gcc",
        *FULLBENCH_FLAGS,
        str(TESTS / "fullbench.c"),
        str(STATIC_LIB),
        *RUST_LINK_LIBS,
        "-o",
        str(destination),
    ]
    run(command)


def generate_corpus(datagen: Path, destination: Path, probability: int) -> None:
    log(f"Generating {destination.name}")
    with destination.open("wb") as output:
        subprocess.run(
            [str(datagen), "-g8M", f"-P{probability}"],
            cwd=ROOT,
            check=True,
            stdout=output,
        )


def generate_corpora(work: Path) -> dict[str, Path]:
    run(["make", "-C", str(TESTS), "datagen"])
    datagen = (TESTS / "datagen").resolve(strict=True)
    corpora = {
        "P10": work / "d10.bin",
        "P50": work / "d50.bin",
        "P90": work / "d90.bin",
        "zeros": work / "zeros.bin",
    }
    generate_corpus(datagen, corpora["P10"], 10)
    generate_corpus(datagen, corpora["P50"], 50)
    generate_corpus(datagen, corpora["P90"], 90)
    with corpora["zeros"].open("wb") as output:
        output.truncate(8 * 1024 * 1024)
    latency_input = work / "d50-1m.bin"
    with latency_input.open("wb") as output:
        subprocess.run(
            [str(datagen), "-g1M", "-P50"],
            cwd=ROOT,
            check=True,
            stdout=output,
        )
    corpora["latency"] = latency_input
    return corpora


def build_artifacts(work: Path) -> dict[str, Path]:
    remove_generated_builds()
    work.mkdir(parents=True, exist_ok=True)
    corpora = generate_corpora(work)

    fullbench_c = work / "fullbench-c"
    fullbench_rust = work / "fullbench-rust"
    cli_c = work / "lz4-c"
    cli_rust = work / "lz4-rust"

    compile_fullbench_c(fullbench_c)
    run(["make", "-C", str(PROGRAMS), "lz4"])
    copy_resolved(PROGRAMS / "lz4", cli_c)

    run(["cargo", "build", "--release"])
    compile_fullbench_rust(fullbench_rust)

    shutil.rmtree(PROGRAMS / "cachedObjs", ignore_errors=True)
    for name in ("lz4", "lz4c", "lz4cat", "unlz4"):
        path = PROGRAMS / name
        if path.is_symlink():
            path.unlink()
    run(["make", "cli"])
    provenance = capture(["make", "provenance-check"])
    log(provenance.strip())
    copy_resolved(PROGRAMS / "lz4", cli_rust)

    return {
        "fullbench_c": fullbench_c,
        "fullbench_rust": fullbench_rust,
        "cli_c": cli_c,
        "cli_rust": cli_rust,
        **corpora,
    }


def fullbench_speed(binary: Path, algorithm: str, corpus: Path, inner_iterations: int) -> float:
    output = capture(
        [str(binary), f"-i{inner_iterations}", f"-{algorithm}", str(corpus)],
        timeout=300,
        quiet=True,
    ).replace("\r", "\n")
    speeds = SPEED_PATTERN.findall(output)
    if not speeds:
        raise RuntimeError(f"no throughput found for {binary.name} {algorithm} {corpus.name}")
    return float(speeds[-1])


def paired_speeds(
    c_binary: Path,
    rust_binary: Path,
    algorithm: str,
    corpus: Path,
    repetitions: int,
    inner_iterations: int,
) -> tuple[float, float]:
    c_speeds = []
    rust_speeds = []
    for repetition in range(repetitions):
        if repetition % 2 == 0:
            c_speeds.append(fullbench_speed(c_binary, algorithm, corpus, inner_iterations))
            rust_speeds.append(fullbench_speed(rust_binary, algorithm, corpus, inner_iterations))
        else:
            rust_speeds.append(fullbench_speed(rust_binary, algorithm, corpus, inner_iterations))
            c_speeds.append(fullbench_speed(c_binary, algorithm, corpus, inner_iterations))
    c_result = max(c_speeds)
    rust_result = max(rust_speeds)
    log(f"{corpus.name:12} {algorithm:3} C {c_result:10.1f}  Rust {rust_result:10.1f} MB/s")
    return c_result, rust_result


def ratio(rust: float, c: float) -> float:
    return round(rust / c, 2)


def measure_throughput(artifacts: dict[str, Path], repetitions: int, inner_iterations: int) -> dict:
    c_binary = artifacts["fullbench_c"]
    rust_binary = artifacts["fullbench_rust"]
    p50 = artifacts["P50"]
    hot = {}
    algorithm_names = {
        "c1": "compress_default",
        "c9": "compress_fast_continue",
        "d1": "decompress_fast",
        "d4": "decompress_safe",
    }
    for algorithm, name in algorithm_names.items():
        c_speed, rust_speed = paired_speeds(
            c_binary,
            rust_binary,
            algorithm,
            p50,
            repetitions,
            inner_iterations,
        )
        hot[name] = {
            "c": c_speed,
            "rust": rust_speed,
            "rust_to_c_ratio": ratio(rust_speed, c_speed),
        }

    corpus_results = {}
    for corpus_name in ("P10", "P50", "P90", "zeros"):
        corpus = artifacts[corpus_name]
        if corpus_name == "P50":
            c_compress = hot["compress_default"]["c"]
            rust_compress = hot["compress_default"]["rust"]
            c_decompress = hot["decompress_safe"]["c"]
            rust_decompress = hot["decompress_safe"]["rust"]
        else:
            c_compress, rust_compress = paired_speeds(
                c_binary,
                rust_binary,
                "c1",
                corpus,
                repetitions,
                inner_iterations,
            )
            c_decompress, rust_decompress = paired_speeds(
                c_binary,
                rust_binary,
                "d4",
                corpus,
                repetitions,
                inner_iterations,
            )
        corpus_results[corpus_name] = {
            "c_compress": c_compress,
            "rust_compress": rust_compress,
            "c_decompress": c_decompress,
            "rust_decompress": rust_decompress,
        }
    return {"hot_loop_p50_corpus": hot, "corpus": corpus_results}


def latency_summary(values: list[float]) -> dict[str, float]:
    return {
        "p50": round(statistics.median(values), 1),
        "max": round(max(values), 1),
    }


def measure_latency(artifacts: dict[str, Path], trials: int) -> dict:
    input_size = artifacts["latency"].stat().st_size
    results = {}
    for algorithm, name in (("c1", "compress_default"), ("d4", "decompress_safe")):
        c_latencies = []
        rust_latencies = []
        for trial in range(trials):
            if trial % 2 == 0:
                c_speed = fullbench_speed(artifacts["fullbench_c"], algorithm, artifacts["latency"], 1)
                rust_speed = fullbench_speed(artifacts["fullbench_rust"], algorithm, artifacts["latency"], 1)
            else:
                rust_speed = fullbench_speed(artifacts["fullbench_rust"], algorithm, artifacts["latency"], 1)
                c_speed = fullbench_speed(artifacts["fullbench_c"], algorithm, artifacts["latency"], 1)
            c_latencies.append(input_size / c_speed)
            rust_latencies.append(input_size / rust_speed)
        c_summary = latency_summary(c_latencies)
        rust_summary = latency_summary(rust_latencies)
        results[name] = {
            "c_p50": c_summary["p50"],
            "c_max": c_summary["max"],
            "rust_p50": rust_summary["p50"],
            "rust_max": rust_summary["max"],
        }
        log(f"latency {name:24} C {c_summary['p50']:8.1f} us  Rust {rust_summary['p50']:8.1f} us")
    return results


def wait4_max_rss(command: list[str]) -> int:
    pid = os.fork()
    if pid == 0:
        try:
            devnull = os.open(os.devnull, os.O_RDWR)
            os.dup2(devnull, 0)
            os.dup2(devnull, 1)
            os.dup2(devnull, 2)
            os.execvpe(command[0], command, os.environ.copy())
        except BaseException:
            os._exit(127)
    _, status, usage = os.wait4(pid, 0)
    exit_code = os.waitstatus_to_exitcode(status)
    if exit_code != 0:
        raise subprocess.CalledProcessError(exit_code, command)
    return usage.ru_maxrss


def measure_memory(artifacts: dict[str, Path], work: Path) -> dict:
    rss = {}
    for label, binary in (("c_cli", artifacts["cli_c"]), ("rust_cli", artifacts["cli_rust"])):
        output = work / f"rss-{label}.lz4"
        output.unlink(missing_ok=True)
        rss_kib = wait4_max_rss([str(binary), "-q", "-f", str(artifacts["P50"]), str(output)])
        output.unlink(missing_ok=True)
        rss[label] = round(rss_kib / 1024, 1)
        log(f"RSS {label:8} {rss[label]:8.1f} MiB")

    stripped = work / "lz4-rust-stripped"
    shutil.copy2(artifacts["cli_rust"], stripped)
    run(["strip", str(stripped)], quiet=True)
    binary_size = {
        "c_cli_kib": round(artifacts["cli_c"].stat().st_size / 1024, 1),
        "rust_cli_unstripped_mebibytes": round(artifacts["cli_rust"].stat().st_size / (1024 * 1024), 2),
        "rust_cli_stripped_mebibytes": round(stripped.stat().st_size / (1024 * 1024), 2),
    }
    return {"peak_rss_mebibytes": rss, "binary_size": binary_size}


def startup_summary(values: list[float]) -> dict[str, float]:
    return {
        "min": round(min(values), 3),
        "p50": round(statistics.median(values), 3),
        "max": round(max(values), 3),
    }


def startup_trial(binary: Path) -> float:
    command = [str(binary), "-q", "-c", os.devnull]
    started = time.perf_counter_ns()
    subprocess.run(command, cwd=ROOT, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return (time.perf_counter_ns() - started) / 1_000_000


def measure_startup(c_binary: Path, rust_binary: Path, trials: int) -> dict:
    for _ in range(2):
        startup_trial(c_binary)
        startup_trial(rust_binary)
    c_values = []
    rust_values = []
    for trial in range(trials):
        if trial % 2 == 0:
            c_values.append(startup_trial(c_binary))
            rust_values.append(startup_trial(rust_binary))
        else:
            rust_values.append(startup_trial(rust_binary))
            c_values.append(startup_trial(c_binary))
    return {
        "c_cli": startup_summary(c_values),
        "rust_cli": startup_summary(rust_values),
    }


def measure_equivalence() -> dict:
    output = capture(["make", "difftest"], timeout=1200)
    match = DIFF_PATTERN.search(output)
    if match is None:
        raise RuntimeError("make difftest did not report byte-identical/diverged totals")
    cases, divergences = (int(value) for value in match.groups())
    log(f"equivalence {cases} cases, {divergences} divergences")
    return {
        "cases": cases,
        "divergences": divergences,
        "command": "make difftest",
        "known_divergence": "Frame API compression levels >= 3 use fast compression rather than HC compression; output remains valid but is larger.",
    }


def first_line(command: list[str]) -> str:
    return capture(command, quiet=True).splitlines()[0]


def version_number(text: str) -> str:
    match = re.search(r"\d+(?:\.\d+)+", text)
    return match.group(0) if match else text


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def environment() -> dict:
    return {
        "host": f"{platform.machine()} {platform.system()} {platform.release()}",
        "rustc": version_number(first_line(["rustc", "--version"])),
        "gcc": version_number(first_line(["gcc", "--version"])),
        "upstream_commit": capture(["git", "-C", str(UPSTREAM), "rev-parse", "HEAD"], quiet=True).strip(),
        "port_artifact": str(STATIC_LIB.relative_to(ROOT)),
        "port_artifact_sha256": sha256(STATIC_LIB),
    }


def write_atomic(output: Path, results: dict) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary_path = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            encoding="utf-8",
            dir=output.parent,
            prefix=f".{output.name}.",
            suffix=".tmp",
            delete=False,
        ) as destination:
            temporary_path = Path(destination.name)
            json.dump(results, destination, indent=2, allow_nan=False)
            destination.write("\n")
            destination.flush()
            os.fsync(destination.fileno())
        os.replace(temporary_path, output)
    finally:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)


def positive(value: str) -> int:
    parsed = int(value)
    if parsed < 1:
        raise argparse.ArgumentTypeError("must be at least 1")
    return parsed


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Build, run, and record the complete C-vs-Rust benchmark suite.")
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT, help="JSON output path")
    parser.add_argument("--work-dir", type=Path, default=DEFAULT_WORK, help="temporary artifact directory")
    parser.add_argument("--repetitions", type=positive, default=3, help="throughput repetitions")
    parser.add_argument("--inner-iterations", type=positive, choices=range(1, 10), default=3, help="fullbench inner iterations")
    parser.add_argument("--latency-trials", type=positive, default=10, help="latency samples")
    parser.add_argument("--startup-trials", type=positive, default=20, help="startup samples")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    require_tools()
    verify_upstream()
    artifacts = build_artifacts(args.work_dir)
    equivalence = measure_equivalence()
    throughput = measure_throughput(artifacts, args.repetitions, args.inner_iterations)
    latency = measure_latency(artifacts, args.latency_trials)
    memory = measure_memory(artifacts, args.work_dir)
    startup = measure_startup(artifacts["cli_c"], artifacts["cli_rust"], args.startup_trials)
    results = {
        "schema_version": 3,
        "measured_at": date.today().isoformat(),
        "generated_by": "bench/bench.py",
        "environment": environment(),
        "methodology": {
            "throughput_input": "8 MiB",
            "throughput_repetitions": args.repetitions,
            "throughput_inner_iterations": args.inner_iterations,
            "throughput_summary": "best_of_runs",
            "latency_input": "1 MiB",
            "latency_trials": args.latency_trials,
            "latency_summary": "fullbench throughput converted to microseconds per 1 MiB call",
            "startup_trials": args.startup_trials,
        },
        "equivalence": equivalence,
        "throughput_mb_per_s": throughput,
        "latency_microseconds": latency,
        "memory": memory,
        "startup_milliseconds": startup,
    }
    write_atomic(args.output, results)
    log(f"Wrote {args.output}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (RuntimeError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        print(f"benchmark failed: {error}", file=sys.stderr)
        raise SystemExit(1)
