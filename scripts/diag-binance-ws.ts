#!/usr/bin/env bun
// Diagnostic v4: per-stream isolation tests, but using the ROUTED
// /market/ws/ URL form that the Binance Futures docs require. Compares
// against the unrouted /ws/ form so we can prove that the missing /market
// segment is what's been hiding kline + aggTrade events.
//
// Run: bun scripts/diag-binance-ws.ts

const STREAMS = ["btcusdt@kline_1m", "btcusdt@aggTrade", "btcusdt@depth@100ms"];
const DURATION_MS = Number(process.env.DURATION_MS ?? 15000);

type Variant = "unrouted" | "routed-market" | "routed-public";

interface Result {
  variant: Variant;
  stream: string;
  url: string;
  events: number;
  firstEventDelayMs?: number;
  firstSample?: string;
  closeCode?: number;
  closeReason?: string;
}

function buildUrl(variant: Variant, stream: string): string {
  switch (variant) {
    case "unrouted":      return `wss://fstream.binance.com/ws/${stream}`;
    case "routed-market": return `wss://fstream.binance.com/market/ws/${stream}`;
    case "routed-public": return `wss://fstream.binance.com/public/ws/${stream}`;
  }
}

async function probe(variant: Variant, stream: string): Promise<Result> {
  const url = buildUrl(variant, stream);
  return new Promise((resolve) => {
    const out: Result = { variant, stream, url, events: 0 };
    const ws = new WebSocket(url);
    let openedAt: number | undefined;
    ws.addEventListener("open", () => { openedAt = Date.now(); });
    ws.addEventListener("message", (ev) => {
      out.events++;
      if (!out.firstSample) {
        out.firstSample = String(ev.data).slice(0, 200);
        if (openedAt) out.firstEventDelayMs = Date.now() - openedAt;
      }
    });
    ws.addEventListener("close", (e) => {
      out.closeCode = (e as CloseEvent).code;
      out.closeReason = (e as CloseEvent).reason;
    });
    setTimeout(() => {
      try { ws.close(); } catch { /* */ }
      resolve(out);
    }, DURATION_MS);
  });
}

const variants: Variant[] = ["unrouted", "routed-market", "routed-public"];

console.log(`[diag] probing ${variants.length} URL variants × ${STREAMS.length} streams for ${DURATION_MS}ms each (parallel)`);
console.log("");

const all: Result[] = [];
for (const v of variants) {
  console.log(`[diag] launching ${v} batch`);
  const batch = await Promise.all(STREAMS.map(s => probe(v, s)));
  all.push(...batch);
}

console.log("\n[diag] === results matrix ===");
console.log("variant         stream                events  first(ms)  close");
for (const r of all) {
  const status = r.events > 0 ? String(r.events).padStart(6) : "  ZERO";
  const delay = r.firstEventDelayMs != null ? String(r.firstEventDelayMs).padStart(9) : "        -";
  const close = `${r.closeCode ?? "?"}${r.closeReason ? ` (${r.closeReason})` : ""}`;
  console.log(`  ${r.variant.padEnd(15)} ${r.stream.padEnd(22)} ${status}  ${delay}  ${close}`);
}

console.log("\n[diag] === interpretation ===");
const byVariant = (v: Variant) => all.filter(r => r.variant === v);
const klineHit = (v: Variant) => (byVariant(v).find(r => r.stream === "btcusdt@kline_1m")?.events ?? 0) > 0;
const tradeHit = (v: Variant) => (byVariant(v).find(r => r.stream === "btcusdt@aggTrade")?.events ?? 0) > 0;
const depthHit = (v: Variant) => (byVariant(v).find(r => r.stream === "btcusdt@depth@100ms")?.events ?? 0) > 0;

for (const v of variants) {
  const k = klineHit(v), t = tradeHit(v), d = depthHit(v);
  const tag = k && t && d ? "FULL"
            : k || t || d ? "PARTIAL"
            :               "NONE";
  console.log(`  ${v.padEnd(15)} → kline=${k ? "Y" : "N"}  aggTrade=${t ? "Y" : "N"}  depth=${d ? "Y" : "N"}  [${tag}]`);
}

if (klineHit("routed-market") && tradeHit("routed-market") && depthHit("routed-market")) {
  console.log("\n  ✓ /market/ws works. Server fix: add '/market' to combined_url().");
} else if (klineHit("routed-public") && tradeHit("routed-public") && depthHit("routed-public")) {
  console.log("\n  ✓ /public/ws works. Server fix: use /public instead of bare /ws.");
}

process.exit(0);
