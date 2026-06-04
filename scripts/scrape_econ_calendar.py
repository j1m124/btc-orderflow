#!/usr/bin/env python3
"""
Scrape forexfactory.com/calendar for the previous 15 + next 15 days and emit
crates/terminal_demo/src/economic_calendar_data.rs.

ForexFactory is behind Cloudflare's TLS-fingerprint challenge, so plain curl
returns 403. cloudscraper passes the challenge.

Run:
    pip install --user cloudscraper
    python3 scripts/scrape_econ_calendar.py [YYYY-MM-DD]   # defaults to today

The "today" arg lets you reproduce a specific snapshot — useful when the demo
expects events around a known date.
"""
import json
import os
import re
import sys
import time
from collections import Counter
from datetime import date, datetime, timedelta, timezone
from html import unescape
from pathlib import Path

import cloudscraper

REPO_ROOT = Path(__file__).resolve().parent.parent
OUT = REPO_ROOT / "crates" / "terminal_demo" / "src" / "economic_calendar" / "data.rs"

MONTHS = ["jan","feb","mar","apr","may","jun","jul","aug","sep","oct","nov","dec"]


def sunday_on_or_before(d: date) -> date:
    return d - timedelta(days=(d.weekday() + 1) % 7)


def url_for(d: date) -> str:
    return f"https://www.forexfactory.com/calendar?week={MONTHS[d.month-1]}{d.day}.{d.year}"


def parse_rows(html: str):
    out = []
    for chunk in re.split(r"<tr\b", html)[1:]:
        end = chunk.find("</tr>")
        if end == -1:
            continue
        gt = chunk.find(">")
        out.append((chunk[:gt], chunk[gt + 1 : end]))
    return out


def text_of(html: str) -> str:
    return re.sub(r"\s+", " ", unescape(re.sub(r"<[^>]+>", "", html)).strip())


IMPACT_BY_CODE = {"gra": "Holiday", "yel": "Low", "ora": "Medium", "red": "High"}


def parse_week(html: str):
    events = []
    last_date = None
    last_time = ""
    for attrs, body in parse_rows(html):
        if "data-event-id" not in attrs:
            continue
        m = re.search(r'data-day-dateline="(\d+)"', attrs)
        if m:
            last_date = datetime.fromtimestamp(int(m.group(1)), tz=timezone.utc).date()
        if last_date is None:
            continue
        tm = re.search(r'class="calendar__cell calendar__time[^"]*">([^<]*)<', body)
        time_str = unescape(tm.group(1)).strip() if tm else ""
        if time_str:
            last_time = time_str
        else:
            time_str = last_time
        cm = re.search(r'class="calendar__cell calendar__currency[^"]*">([^<]*)<', body)
        currency = unescape(cm.group(1)).strip() if cm else ""
        im = re.search(r"icon icon--ff-impact-(\w+)", body)
        impact = IMPACT_BY_CODE.get(im.group(1) if im else "", "")
        ttl = re.search(r'class="calendar__event-title">([^<]*)<', body)
        title = unescape(ttl.group(1)).strip() if ttl else ""
        def cell(name):
            mm = re.search(r'class="calendar__cell calendar__' + name + r'[^"]*">(.*?)</td>', body, re.S)
            return text_of(mm.group(1)) if mm else ""
        events.append(
            {
                "date": last_date.isoformat(),
                "time": time_str,
                "currency": currency,
                "impact": impact,
                "title": title,
                "actual": cell("actual"),
                "forecast": cell("forecast"),
                "previous": cell("previous"),
            }
        )
    return events


IMPACT_RUST = {
    "High": "Impact::High",
    "Medium": "Impact::Medium",
    "Low": "Impact::Low",
    "Holiday": "Impact::Holiday",
}


def emit_rust(events, today_iso, from_iso, to_iso):
    def esc(s):
        return s.replace("\\", "\\\\").replace('"', '\\"')

    lines = [
        f"// Generated from forexfactory.com/calendar via cloudscraper on {today_iso}.",
        f"// Range: {from_iso} .. {to_iso} (15 days before and after {today_iso}).",
        "// Do NOT hand-edit; rerun scripts/scrape_econ_calendar.py to refresh.",
        "",
        "use super::{EconomicEvent, Impact};",
        "",
        "pub const EVENTS: &[EconomicEvent] = &[",
    ]
    for e in events:
        impact = IMPACT_RUST.get(e["impact"], "Impact::Low")
        lines.append(
            f'    EconomicEvent {{ date: "{e["date"]}", time: "{esc(e["time"])}", '
            f'currency: "{esc(e["currency"])}", impact: {impact}, '
            f'title: "{esc(e["title"])}", actual: "{esc(e["actual"])}", '
            f'forecast: "{esc(e["forecast"])}", previous: "{esc(e["previous"])}" }},'
        )
    lines.append("];")
    lines.append("")
    OUT.write_text("\n".join(lines))


def main(argv):
    today = date.fromisoformat(argv[1]) if len(argv) > 1 else date.today()
    from_d = today - timedelta(days=15)
    to_d = today + timedelta(days=15)
    base_sun = sunday_on_or_before(today)
    sundays = [base_sun + timedelta(weeks=i) for i in (-2, -1, 0, 1, 2)]

    scraper = cloudscraper.create_scraper(
        browser={"browser": "chrome", "platform": "darwin", "desktop": True}
    )
    print(f"Today: {today}, range: {from_d}..{to_d}, fetching {len(sundays)} weeks")

    all_events = []
    seen = set()
    for s in sundays:
        url = url_for(s)
        print(f"  GET {url}")
        r = scraper.get(url, timeout=30)
        if r.status_code != 200:
            print(f"    !! HTTP {r.status_code}, skipping")
            continue
        for e in parse_week(r.text):
            key = (e["date"], e["time"], e["currency"], e["title"])
            if key in seen:
                continue
            seen.add(key)
            all_events.append(e)
        time.sleep(2)

    all_events = [e for e in all_events if from_d.isoformat() <= e["date"] <= to_d.isoformat()]
    all_events.sort(key=lambda e: (e["date"], e["time"]))

    print(f"Total events in range: {len(all_events)}")
    print(f"  By impact:   {dict(Counter(e['impact'] for e in all_events))}")
    print(f"  By currency: {dict(Counter(e['currency'] for e in all_events).most_common())}")

    emit_rust(all_events, today.isoformat(), from_d.isoformat(), to_d.isoformat())
    print(f"Wrote {OUT}")


if __name__ == "__main__":
    main(sys.argv)
