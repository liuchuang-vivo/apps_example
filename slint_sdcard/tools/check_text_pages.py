#!/usr/bin/env python3
"""Run the SD reader's hardware-independent Rust pagination tests on the host."""

import argparse
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    binary = args.output.with_suffix(".bin").resolve()
    subprocess.run(["rustc", "--edition=2021", "--test", str(args.source),
                    "-o", str(binary)], check=True)
    subprocess.run([str(binary)], check=True)
    args.output.write_text("SD text pagination tests passed\n", encoding="utf-8")


if __name__ == "__main__":
    main()
