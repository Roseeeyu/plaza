#!/usr/bin/env python3
"""Fan-out benchmark for Plaza's raw line protocol.

Opens N subscribers to the whole canvas, reports the server's OS thread count
and resident memory, then times one 8x8 stamp reaching every subscriber.

    cargo run --release -- --tcp 127.0.0.1:8081 --no-persist --lag-budget 100000 &
    ulimit -n 4096
    python3 bench/fanout.py <server pid> [--port 8081] [--clients 1000]

Thread and memory figures come from `ps`, so this runs on macOS and Linux.
"""

import argparse
import platform
import socket
import subprocess
import time

STAMP = 8  # stamp side, in pixels


def thread_count(pid: int) -> int:
    if platform.system() == "Darwin":
        out = subprocess.run(["ps", "-M", "-p", str(pid)], capture_output=True, text=True).stdout
        return out.count("\n") - 1
    out = subprocess.run(["ps", "-o", "nlwp=", "-p", str(pid)], capture_output=True, text=True).stdout
    return int(out.strip())


def rss_mb(pid: int) -> int:
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True).stdout
    return int(out.strip()) // 1024


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("pid", type=int, help="server process id")
    parser.add_argument("--port", type=int, default=8081)
    parser.add_argument("--clients", type=int, default=1000)
    args = parser.parse_args()

    subscribers = []
    for _ in range(args.clients):
        s = socket.create_connection(("127.0.0.1", args.port))
        s.sendall(b"subscribe 0 0 64 64\n")
        subscribers.append(s)
    for s in subscribers:
        s.settimeout(10)
        s.recv(64)  # "subscribed <seq>"

    print(f"{args.clients} subscribed connections: "
          f"{thread_count(args.pid)} OS threads, {rss_mb(args.pid)} MB RSS")

    painter = socket.create_connection(("127.0.0.1", args.port))
    painter.sendall(b"hello bench\n")
    painter.recv(64)

    start = time.perf_counter()
    painter.sendall(f"stamp 0 0 {STAMP} {STAMP} {'5' * STAMP * STAMP}\n".encode())
    for s in subscribers:
        lines = 0
        while lines < STAMP * STAMP:
            lines += s.recv(65536).count(b"\n")
    elapsed = (time.perf_counter() - start) * 1000

    print(f"one {STAMP}x{STAMP} stamp reached all {args.clients} subscribers "
          f"({args.clients * STAMP * STAMP} diffs) in {elapsed:.0f} ms")


if __name__ == "__main__":
    main()
