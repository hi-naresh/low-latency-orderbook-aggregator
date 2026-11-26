#!/usr/bin/env python3
"""Record real market-data frames from Binance and Bybit as test fixtures.

Writes one raw WebSocket text frame per line, exactly as received (both venues send
compact single-line JSON), into crates/ob-parse/tests/fixtures/. The parser tests replay
these, so they check against what the exchanges really send, not hand-written samples.

Usage (from rust/):  python3 scripts/record_fixtures.py [frames_per_venue]
Requires: pip install websockets
"""
import asyncio
import json
import pathlib
import sys

import websockets

OUT = pathlib.Path(__file__).resolve().parent.parent / "crates/ob-parse/tests/fixtures"

VENUES = {
    # file: (url, subscription, frames). Same subscriptions as the Java skeleton's clients...
    "binance_depth20.jsonl": (
        "wss://fstream.binance.com/ws",
        {"method": "SUBSCRIBE", "params": ["btcusdt@depth20@100ms"], "id": 1},
        None,
    ),
    "bybit_orderbook50.jsonl": (
        "wss://stream.bybit.com/v5/public/linear",
        {"req_id": "depth", "op": "subscribe", "args": ["orderbook.50.BTCUSDT"]},
        None,
    ),
    # ...and the real-time top-of-book streams, alone and interleaved with depth
    # on one connection (the interleaved files are longer: bookTicker outnumbers depth
    # ~100:1, so 1000 frames hold only a few depth messages).
    "binance_bookticker.jsonl": (
        "wss://fstream.binance.com/ws",
        {"method": "SUBSCRIBE", "params": ["btcusdt@bookTicker"], "id": 1},
        None,
    ),
    "bybit_orderbook1.jsonl": (
        "wss://stream.bybit.com/v5/public/linear",
        {"req_id": "sub", "op": "subscribe", "args": ["orderbook.1.BTCUSDT"]},
        None,
    ),
    "binance_depth_and_bookticker.jsonl": (
        "wss://fstream.binance.com/ws",
        {"method": "SUBSCRIBE", "params": ["btcusdt@depth20@100ms", "btcusdt@bookTicker"], "id": 1},
        1000,
    ),
    "bybit_orderbook50_and_1.jsonl": (
        "wss://stream.bybit.com/v5/public/linear",
        {"req_id": "sub", "op": "subscribe", "args": ["orderbook.50.BTCUSDT", "orderbook.1.BTCUSDT"]},
        150,
    ),
}


async def record(url, subscribe, count):
    frames = []
    async with websockets.connect(url, max_size=2**22) as ws:
        await ws.send(json.dumps(subscribe))
        while len(frames) < count:
            frame = await asyncio.wait_for(ws.recv(), timeout=30)
            if isinstance(frame, str):
                assert "\n" not in frame, "expected single-line JSON frames"
                frames.append(frame)
    return frames


async def main():
    count = int(sys.argv[1]) if len(sys.argv) > 1 else 60
    OUT.mkdir(parents=True, exist_ok=True)
    for name, (url, subscribe, frames_for_file) in VENUES.items():
        frames = await record(url, subscribe, frames_for_file or count)
        (OUT / name).write_text("\n".join(frames) + "\n")
        print(f"{name}: {len(frames)} frames, {sum(map(len, frames))} bytes")


asyncio.run(main())
