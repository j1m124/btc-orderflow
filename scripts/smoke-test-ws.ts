#!/usr/bin/env bun
// WebSocket round-trip tester for every channel on the gateway.
//
// For each (Candles, Trades, Footprint, Book) subscription:
//   - subscribe; assert a *Snapshot frame arrives
//   - collect frames for COLLECT_MS; assert at least one *Tick/*Update/*Delta
//   - send a HistoryPage; assert the matching *HistoryPage frame arrives
//   - unsubscribe
//
// Plus error-path tests: bad symbol, bad footprint bucket, history on
// unknown id, ping → pong.
//
// stdout is captured by the calling shell (smoke-test.sh) into the main log.
// Exit code: 0 if all assertions pass, 1 if any fail.

const WS_URL = process.env.WS_URL || "ws://127.0.0.1:8787/ws";
const SYMBOL = process.env.SYMBOL || "BTCUSDT";
const COLLECT_MS = Number(process.env.COLLECT_MS ?? 6000);
const HISTORY_WAIT_MS = Number(process.env.HISTORY_WAIT_MS ?? 4000);
const CONNECT_TIMEOUT_MS = 8000;

interface ChannelSpec {
  name: string;
  channel: Record<string, unknown>;
  snapshotType: string;
  liveType: string;
  historyType: string;
}

const channels: ChannelSpec[] = [
  {
    name: "Candles M5 (native kline)",
    channel: { kind: "candles", tf: "5m" },
    snapshotType: "snapshot",
    liveType: "tick",
    historyType: "history_page",
  },
  {
    name: "Candles M1 (native kline)",
    channel: { kind: "candles", tf: "1m" },
    snapshotType: "snapshot",
    liveType: "tick",
    historyType: "history_page",
  },
  {
    name: "Candles S5 (synthesized from trades)",
    channel: { kind: "candles", tf: "5s" },
    snapshotType: "snapshot",
    liveType: "tick",
    historyType: "history_page",
  },
  {
    name: "Candles S1 (synthesized from trades)",
    channel: { kind: "candles", tf: "1s" },
    snapshotType: "snapshot",
    liveType: "tick",
    historyType: "history_page",
  },
  {
    name: "Trades (raw aggTrade)",
    channel: { kind: "trades" },
    snapshotType: "trade_snapshot",
    liveType: "trade_tick",
    historyType: "trade_history_page",
  },
  {
    name: "Footprint M1 $1 buckets",
    channel: { kind: "footprint", tf: "1m", price_bucket: 1.0 },
    snapshotType: "footprint_snapshot",
    liveType: "footprint_update",
    historyType: "footprint_history_page",
  },
  {
    name: "Book depth=50",
    channel: { kind: "book", depth: 50 },
    snapshotType: "book_snapshot",
    liveType: "book_delta",
    historyType: "book_history_page",
  },
];

type Frame = { type?: string; id?: number; code?: string; msg?: string; [k: string]: unknown };

const results = { pass: 0, fail: 0, failed: [] as string[] };

function ts() {
  return new Date().toISOString().slice(11, 23);
}
function log(msg: string) { console.log(`[ws ${ts()}] ${msg}`); }
function pass(name: string, details: string) { results.pass++; log(`PASS  ${name}: ${details}`); }
function fail(name: string, details: string) { results.fail++; results.failed.push(name); log(`FAIL  ${name}: ${details}`); }
function sleep(ms: number) { return new Promise<void>(r => setTimeout(r, ms)); }

function connect(url: string): Promise<WebSocket> {
  return new Promise((resolve, reject) => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    const ws = new WebSocket(url);
    timer = setTimeout(() => {
      try { ws.close(); } catch { /* ignore */ }
      reject(new Error(`connect timeout after ${CONNECT_TIMEOUT_MS}ms`));
    }, CONNECT_TIMEOUT_MS);
    ws.addEventListener("open", () => { if (timer) clearTimeout(timer); resolve(ws); }, { once: true });
    ws.addEventListener("error", (e) => { if (timer) clearTimeout(timer); reject(e); }, { once: true });
  });
}

function describeFrame(frame: Frame): string {
  const t = frame.type;
  if (t === "snapshot")              return `${(frame.candles as unknown[] | undefined)?.length ?? 0} candles, server_v=${frame.server_v}`;
  if (t === "tick")                  return `tick close=${(frame.candle as Record<string, unknown> | undefined)?.close} closed=${frame.is_closed}`;
  if (t === "history_page")          return `${(frame.candles as unknown[] | undefined)?.length ?? 0} historical candles`;
  if (t === "trade_snapshot")        return `${(frame.trades as unknown[] | undefined)?.length ?? 0} trades, server_v=${frame.server_v}`;
  if (t === "trade_tick")            return `${(frame.trades as unknown[] | undefined)?.length ?? 0} trades`;
  if (t === "trade_history_page")    return `${(frame.trades as unknown[] | undefined)?.length ?? 0} historical trades`;
  if (t === "footprint_snapshot")    return `${(frame.cells as unknown[] | undefined)?.length ?? 0} cells, server_v=${frame.server_v}`;
  if (t === "footprint_update")      return `${(frame.cells as unknown[] | undefined)?.length ?? 0} cells`;
  if (t === "footprint_history_page") return `${(frame.cells as unknown[] | undefined)?.length ?? 0} historical cells`;
  if (t === "book_snapshot")         return `${(frame.bids as unknown[] | undefined)?.length ?? 0} bids / ${(frame.asks as unknown[] | undefined)?.length ?? 0} asks, server_v=${frame.server_v}`;
  if (t === "book_delta")            return `${(frame.bids as unknown[] | undefined)?.length ?? 0} bid changes / ${(frame.asks as unknown[] | undefined)?.length ?? 0} ask changes`;
  if (t === "book_history_page")     return `${(frame.snapshots as unknown[] | undefined)?.length ?? 0} historical snapshots`;
  return JSON.stringify(frame).slice(0, 240);
}

function pickHistoryCursor(spec: ChannelSpec, snapshot: Frame | undefined): number | null {
  if (!snapshot) return null;
  switch (spec.snapshotType) {
    case "snapshot": {
      const c = snapshot.candles as Array<{ open_time: number }> | undefined;
      return c?.[0]?.open_time ?? null;
    }
    case "trade_snapshot": {
      const t = snapshot.trades as Array<{ ts_ms: number }> | undefined;
      return t?.[0]?.ts_ms ?? null;
    }
    case "footprint_snapshot": {
      const c = snapshot.cells as Array<{ open_time: number }> | undefined;
      return c?.[0]?.open_time ?? null;
    }
    case "book_snapshot":
      // The snapshot itself has no ts on the wire; pick "now - 5s" so the
      // server pages anything persisted before then. With a 1s snapshot
      // cadence and a populated table this should land several rows.
      return Date.now() - 5_000;
    default:
      return null;
  }
}

async function testChannel(spec: ChannelSpec, subId: number): Promise<void> {
  log("");
  log(`--- ${spec.name} (id=${subId}) ---`);

  let ws: WebSocket;
  try {
    ws = await connect(WS_URL);
  } catch (e) {
    fail(spec.name, `connect: ${(e as Error)?.message ?? e}`);
    return;
  }

  const frames: Frame[] = [];
  let snapshot: Frame | undefined;
  let historyCountBefore = 0;

  ws.addEventListener("message", (ev) => {
    let frame: Frame;
    try {
      frame = JSON.parse(ev.data as string) as Frame;
    } catch (err) {
      log(`  decode error: ${(err as Error).message}`);
      return;
    }
    if (frame.id !== subId) return;
    frames.push(frame);
    if (frame.type === spec.snapshotType && !snapshot) snapshot = frame;
  });

  ws.send(JSON.stringify({
    op: "subscribe", id: subId, symbol: SYMBOL, channel: spec.channel,
  }));

  await sleep(COLLECT_MS);

  // -- snapshot assertion
  if (snapshot) {
    pass(`${spec.name} subscribe`, describeFrame(snapshot));
  } else {
    const types = frames.map(f => f.type).join(", ");
    fail(`${spec.name} subscribe`, `no '${spec.snapshotType}' received in ${COLLECT_MS}ms. got: [${types}]`);
  }

  // -- live assertion
  const liveFrames = frames.filter(f => f.type === spec.liveType);
  if (liveFrames.length > 0) {
    const lastLive = liveFrames[liveFrames.length - 1];
    pass(`${spec.name} live (${liveFrames.length} frames)`, describeFrame(lastLive));
  } else {
    fail(`${spec.name} live`, `no '${spec.liveType}' in ${COLLECT_MS}ms (live tick rate ≈ 10 Hz; if Binance just connected, retry)`);
  }

  // -- history page assertion
  historyCountBefore = frames.filter(f => f.type === spec.historyType).length;
  const beforeMs = pickHistoryCursor(spec, snapshot);
  if (beforeMs == null) {
    log(`  skipping history-page check (no cursor available)`);
  } else {
    ws.send(JSON.stringify({
      op: "history_page", id: subId, before_ms: beforeMs, count: 100,
    }));
    await sleep(HISTORY_WAIT_MS);
    const hpAfter = frames.filter(f => f.type === spec.historyType);
    if (hpAfter.length > historyCountBefore) {
      const last = hpAfter[hpAfter.length - 1];
      pass(`${spec.name} history`, describeFrame(last));
    } else {
      fail(`${spec.name} history`, `no '${spec.historyType}' after request (before_ms=${beforeMs})`);
    }
  }

  // -- unsubscribe
  ws.send(JSON.stringify({ op: "unsubscribe", id: subId }));
  await sleep(500);
  try { ws.close(); } catch { /* ignore */ }
  await sleep(150);
}

async function testErrorsAndPing(): Promise<void> {
  log("");
  log("--- Error cases + ping ---");
  let ws: WebSocket;
  try {
    ws = await connect(WS_URL);
  } catch (e) {
    fail("error-path setup", `connect: ${(e as Error)?.message ?? e}`);
    return;
  }
  const errors: Frame[] = [];
  const pongs: Frame[] = [];
  ws.addEventListener("message", (ev) => {
    let f: Frame;
    try { f = JSON.parse(ev.data as string) as Frame; } catch { return; }
    if (f.type === "error") errors.push(f);
    if (f.type === "pong")  pongs.push(f);
  });

  // 1. Bad symbol
  ws.send(JSON.stringify({
    op: "subscribe", id: 1000, symbol: "NOTREAL",
    channel: { kind: "candles", tf: "1m" },
  }));
  // 2. Bad footprint bucket
  ws.send(JSON.stringify({
    op: "subscribe", id: 1001, symbol: SYMBOL,
    channel: { kind: "footprint", tf: "1m", price_bucket: -1 },
  }));
  // 3. History on unknown sub id
  ws.send(JSON.stringify({
    op: "history_page", id: 9999, before_ms: 0, count: 10,
  }));
  // 4. Ping/Pong round-trip
  const pingTs = Date.now();
  ws.send(JSON.stringify({ op: "ping", ts_ms: pingTs }));

  await sleep(2500);

  const codes = errors.map(e => e.code).join(", ");
  log(`  errors received: [${codes}]`);
  if (errors.some(e => e.code === "unknown_symbol")) {
    pass("error: bad symbol", String(errors.find(e => e.code === "unknown_symbol")!.msg));
  } else {
    fail("error: bad symbol", "no 'unknown_symbol' error frame");
  }
  if (errors.some(e => e.code === "invalid_bucket")) {
    pass("error: bad footprint bucket", String(errors.find(e => e.code === "invalid_bucket")!.msg));
  } else {
    fail("error: bad footprint bucket", "no 'invalid_bucket' error frame");
  }
  if (errors.some(e => e.code === "unknown_subscription")) {
    pass("error: history for unknown id", String(errors.find(e => e.code === "unknown_subscription")!.msg));
  } else {
    fail("error: history for unknown id", "no 'unknown_subscription' error frame");
  }
  if (pongs.length > 0) {
    const echoed = pongs[0].ts_ms === pingTs;
    if (echoed) pass("ping/pong", `pong echoed ts_ms=${pingTs}`);
    else fail("ping/pong", `pong ts_ms=${pongs[0].ts_ms} ≠ ping ts_ms=${pingTs}`);
  } else {
    fail("ping/pong", "no pong received");
  }

  try { ws.close(); } catch { /* ignore */ }
  await sleep(100);
}

async function main() {
  log(`Starting WS smoke test against ${WS_URL}`);
  log(`Symbol=${SYMBOL} COLLECT_MS=${COLLECT_MS} HISTORY_WAIT_MS=${HISTORY_WAIT_MS}`);

  let subId = 1;
  for (const spec of channels) {
    await testChannel(spec, subId++);
  }
  await testErrorsAndPing();

  log("");
  log("============================================================");
  log(`Total: ${results.pass} passed, ${results.fail} failed`);
  if (results.failed.length) {
    log(`Failed: ${results.failed.join("; ")}`);
  }
  log("============================================================");
  process.exit(results.fail === 0 ? 0 : 1);
}

main().catch((e) => {
  log(`fatal: ${(e as Error)?.stack ?? e}`);
  process.exit(2);
});
