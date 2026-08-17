#!/usr/bin/env python3
"""Select one deterministic, disjoint shard of Windows Bazel test targets."""

import argparse
import sys
import zlib


def shard_for_target(target: str, shard_count: int) -> int:
    if shard_count < 1:
        raise ValueError("shard_count must be positive")
    return (zlib.crc32(target.encode()) % shard_count) + 1


def select_targets(targets: list[str], shard: int, shard_count: int) -> list[str]:
    if shard not in range(1, shard_count + 1):
        raise ValueError(f"shard must be between 1 and {shard_count}")
    normalized = sorted(target.strip() for target in targets if target.strip())
    if not normalized:
        raise ValueError("Bazel query returned no Windows test targets")
    if len(normalized) != len(set(normalized)):
        raise ValueError("Bazel query returned duplicate Windows test targets")
    selected = [
        target
        for target in normalized
        if shard_for_target(target, shard_count) == shard
    ]
    if not selected:
        raise ValueError(f"no Windows Bazel targets selected for shard {shard}/{shard_count}")
    return selected


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--shard", type=int, required=True)
    parser.add_argument("--shard-count", type=int, required=True)
    args = parser.parse_args(argv)
    try:
        selected = select_targets(sys.stdin.read().splitlines(), args.shard, args.shard_count)
    except ValueError as error:
        print(error, file=sys.stderr)
        return 1
    sys.stdout.reconfigure(newline="\n")
    print("\n".join(selected))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
