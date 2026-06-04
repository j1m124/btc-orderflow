#!/usr/bin/env python3
"""
Fetch 1h OHLC bars for the chart panel's symbol list from Yahoo Finance and
emit crates/terminal_demo/assets/chart_data.json.

The Rust panel embeds this JSON via include_str!, so re-running this script
and rebuilding gives the demo fresh data.

Run:
    pip install --user yfinance
    python3 scripts/fetch_chart_data.py

The 1h interval is limited to the trailing ~730 days by Yahoo. We pull 60d
which works out to ~290 hourly bars per symbol — enough for the demo's
default 60-bar viewport plus headroom for pan/zoom.
"""
import json
from pathlib import Path

import yfinance as yf

REPO_ROOT = Path(__file__).resolve().parent.parent
OUT = REPO_ROOT / "crates" / "terminal_demo" / "assets" / "chart_data.json"

# (yfinance ticker, display symbol, display name, exchange).
# Yahoo uses dashes for class shares ("BRK-B"); the demo shows them with dots.
SYMBOLS = [
    ("AAPL",  "AAPL",  "Apple Inc.",         "NASDAQ"),
    ("MSFT",  "MSFT",  "Microsoft Corp.",    "NASDAQ"),
    ("NVDA",  "NVDA",  "NVIDIA Corp.",       "NASDAQ"),
    ("GOOGL", "GOOGL", "Alphabet Inc.",      "NASDAQ"),
    ("TSLA",  "TSLA",  "Tesla, Inc.",        "NASDAQ"),
    ("META",  "META",  "Meta Platforms",     "NASDAQ"),
    ("AMZN",  "AMZN",  "Amazon.com",         "NASDAQ"),
    ("BRK-B", "BRK.B", "Berkshire Hathaway", "NYSE"),
]


def fetch(yahoo_symbol: str) -> list[dict]:
    df = yf.Ticker(yahoo_symbol).history(period="60d", interval="1h", auto_adjust=False)
    rows = []
    for ts, row in df.iterrows():
        rows.append({
            "t": ts.strftime("%m/%d %H:%M"),
            "o": round(float(row["Open"]),  2),
            "h": round(float(row["High"]),  2),
            "l": round(float(row["Low"]),   2),
            "c": round(float(row["Close"]), 2),
        })
    return rows


def main() -> None:
    out = {}
    for yahoo, display, name, exchange in SYMBOLS:
        print(f"Fetching {yahoo} → {display}…")
        bars = fetch(yahoo)
        out[display] = {
            "name": name,
            "exchange": exchange,
            "bars": bars,
        }
        print(f"  {len(bars)} bars")

    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(out, separators=(",", ":")))
    print(f"Wrote {OUT} ({OUT.stat().st_size:,} bytes)")


if __name__ == "__main__":
    main()
