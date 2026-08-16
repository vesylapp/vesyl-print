#!/usr/bin/env python3
"""Stream the SPI LCD as MJPEG for local demos.

Used two ways:

1. **Embedded in the display app** (default) — ``main.py`` publishes each
   rendered frame and serves  http://<pi-ip>:8765/

2. **Standalone** (optional) — poll ``/dev/fb1`` without the display loop:

       python3 stream_lcd.py --port 8765 --fps 2 --scale 1

Needs group ``video`` for ``/dev/fb1``. Binds 0.0.0.0 by default (trusted LAN only).

The HTML page shows the LCD at the top and live Pi stats tables below
(pairing, system, printers, jobs, OTA). Stats also available as JSON at
``/api/stats``. Each printer row links to ``/api/test-print`` (PDF, plus ZPL
on raw/Zebra queues).
"""

from __future__ import annotations

import argparse
import io
import json
import logging
import signal
import sys
import threading
import time
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Callable
from urllib.parse import parse_qs, quote, urlparse

from PIL import Image

log = logging.getLogger("vesyl-print.stream")

BOUNDARY = b"frame"
DEFAULT_PORT = 8765
DEFAULT_FPS = 2.0
DEFAULT_SCALE = 1.0
DEFAULT_QUALITY = 80
_STATS_CACHE_S = 2.0
_CLAIM_CODE_LEN = 8
_BASE_DIR = Path(__file__).resolve().parent
_ASSETS_DIR = _BASE_DIR / "assets"

HTML_PAGE = """\
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8"/>
  <meta name="viewport" content="width=device-width, initial-scale=1"/>
  <title>VESYL Print — LCD</title>
  <style>
    :root {{
      --bg: #101218;
      --panel: #181c24;
      --border: #2a303c;
      --fg: #ebeef5;
      --muted: #8c94a5;
      --accent: #edfc33;
      --ok: #50dc78;
      --warn: #ffb43c;
      --down: #e84848;
    }}
    * {{ box-sizing: border-box; }}
    html, body {{
      margin: 0; min-height: 100%; background: var(--bg); color: var(--fg);
      font-family: system-ui, -apple-system, Segoe UI, sans-serif;
    }}
    .page {{
      max-width: 960px;
      margin: 0 auto;
      padding: 16px 16px 40px;
      display: flex;
      flex-direction: column;
      align-items: stretch;
      gap: 16px;
    }}
    header.bar {{
      display: flex; flex-wrap: wrap; align-items: baseline;
      justify-content: space-between; gap: 8px 16px;
    }}
    h1 {{
      font-size: 13px; font-weight: 600; letter-spacing: 0.06em;
      color: var(--muted); margin: 0; text-transform: uppercase;
    }}
    .meta {{ font-size: 12px; color: var(--muted); }}
    a {{ color: var(--accent); text-decoration: none; }}
    a:hover {{ text-decoration: underline; }}
    .lcd-wrap {{
      align-self: flex-start;
      background: #000;
      border: 1px solid var(--border);
      border-radius: 6px;
      box-shadow: 0 8px 28px rgba(0,0,0,0.4);
      overflow: hidden;
      line-height: 0;
    }}
    .lcd-wrap img {{
      image-rendering: pixelated;
      image-rendering: crisp-edges;
      display: block;
      max-width: min(96vw, {css_max}px);
      width: {w}px;
      height: auto;
      background: #000;
    }}
    .claim-panel {{
      background: linear-gradient(180deg, #1a1e28 0%, var(--panel) 100%);
      border: 1px solid var(--border);
      border-radius: 12px;
      padding: 28px 24px 24px;
      text-align: center;
      box-shadow: 0 12px 40px rgba(0,0,0,0.35);
    }}
    .claim-panel[hidden] {{ display: none !important; }}
    .claim-logo {{
      height: 36px; width: auto; margin: 0 auto 18px; display: block;
    }}
    .claim-panel h2 {{
      margin: 0 0 6px;
      font-size: 22px;
      font-weight: 700;
      letter-spacing: -0.02em;
      color: var(--fg);
    }}
    .claim-panel .sub {{
      margin: 0 0 22px;
      font-size: 14px;
      color: var(--muted);
      line-height: 1.45;
    }}
    .claim-panel .sub strong {{ color: var(--accent); font-weight: 600; }}
    .code-row {{
      display: flex;
      align-items: center;
      justify-content: center;
      flex-wrap: wrap;
      gap: 8px;
      margin: 0 auto 18px;
    }}
    .code-group {{
      display: flex;
      gap: 8px;
    }}
    .code-dash {{
      width: 14px;
      height: 3px;
      background: var(--accent);
      border-radius: 2px;
      margin: 0 4px;
      flex-shrink: 0;
    }}
    .code-box {{
      width: 52px;
      height: 64px;
      border: 2px solid var(--border);
      border-radius: 10px;
      background: #0c0e14;
      color: var(--fg);
      font-size: 28px;
      font-weight: 700;
      font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
      text-align: center;
      text-transform: uppercase;
      caret-color: var(--accent);
      outline: none;
      transition: border-color 0.15s, box-shadow 0.15s;
    }}
    .code-box:focus {{
      border-color: var(--accent);
      box-shadow: 0 0 0 3px rgba(237, 252, 51, 0.18);
    }}
    .code-box:disabled {{
      opacity: 0.55;
    }}
    .claim-name {{
      display: flex;
      flex-direction: column;
      align-items: stretch;
      gap: 6px;
      max-width: 360px;
      margin: 0 auto 16px;
      text-align: left;
    }}
    .claim-name label {{
      font-size: 11px;
      font-weight: 600;
      letter-spacing: 0.06em;
      text-transform: uppercase;
      color: var(--muted);
    }}
    .claim-name input {{
      height: 40px;
      border-radius: 8px;
      border: 1px solid var(--border);
      background: #0c0e14;
      color: var(--fg);
      padding: 0 12px;
      font-size: 14px;
      outline: none;
    }}
    .claim-name input:focus {{
      border-color: var(--accent);
    }}
    .claim-actions {{
      display: flex;
      flex-direction: column;
      align-items: center;
      gap: 10px;
    }}
    .btn-claim {{
      appearance: none;
      border: none;
      cursor: pointer;
      background: var(--accent);
      color: #101218;
      font-weight: 700;
      font-size: 15px;
      letter-spacing: 0.02em;
      padding: 12px 28px;
      border-radius: 10px;
      min-width: 200px;
      transition: filter 0.15s, transform 0.1s;
    }}
    .btn-claim:hover:not(:disabled) {{ filter: brightness(1.05); }}
    .btn-claim:active:not(:disabled) {{ transform: scale(0.98); }}
    .btn-claim:disabled {{
      opacity: 0.55;
      cursor: not-allowed;
    }}
    .claim-msg {{
      min-height: 1.25em;
      font-size: 13px;
      margin: 0;
    }}
    .claim-msg.err {{ color: var(--down); }}
    .claim-msg.ok {{ color: var(--ok); }}
    .claim-host {{
      margin-top: 16px;
      font-size: 12px;
      color: var(--muted);
      font-variant-numeric: tabular-nums;
    }}
    @media (max-width: 520px) {{
      .code-box {{ width: 40px; height: 52px; font-size: 22px; }}
      .code-dash {{ margin: 0 2px; }}
    }}
    .stats {{
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
      gap: 12px;
    }}
    section.card {{
      background: var(--panel);
      border: 1px solid var(--border);
      border-radius: 8px;
      padding: 12px 14px 10px;
      min-width: 0;
    }}
    section.card h2 {{
      margin: 0 0 10px;
      font-size: 11px;
      font-weight: 600;
      letter-spacing: 0.08em;
      text-transform: uppercase;
      color: var(--muted);
    }}
    table {{
      width: 100%;
      border-collapse: collapse;
      font-size: 13px;
    }}
    th, td {{
      text-align: left;
      padding: 5px 0;
      vertical-align: top;
      border-bottom: 1px solid rgba(42,48,60,0.65);
    }}
    tr:last-child th, tr:last-child td {{ border-bottom: none; }}
    th {{
      width: 38%;
      color: var(--muted);
      font-weight: 500;
      padding-right: 12px;
    }}
    td {{
      color: var(--fg);
      word-break: break-word;
      font-variant-numeric: tabular-nums;
    }}
    .dot {{
      display: inline-block;
      width: 8px; height: 8px;
      border-radius: 50%;
      margin-right: 6px;
      vertical-align: middle;
    }}
    .dot.ok {{ background: var(--ok); }}
    .dot.warn {{ background: var(--warn); }}
    .dot.down {{ background: var(--down); }}
    .dot.muted {{ background: var(--muted); }}
    .prn-status {{ display: block; }}
    .test-links {{
      display: flex; flex-wrap: wrap; gap: 10px;
      margin-top: 4px;
    }}
    .test-links a {{
      font-size: 12px; font-weight: 600; letter-spacing: 0.04em;
    }}
    .test-links a.busy {{ opacity: 0.45; pointer-events: none; }}
    .print-notice {{ font-size: 12px; color: var(--muted); }}
    .print-notice.ok {{ color: var(--ok); }}
    .print-notice.err {{ color: var(--down); }}
    .empty {{ color: var(--muted); font-size: 13px; margin: 0; }}
    .err {{ color: var(--down); }}
    footer.foot {{
      font-size: 11px; color: var(--muted);
      display: flex; flex-wrap: wrap; gap: 8px 16px;
      justify-content: space-between;
    }}
  </style>
</head>
<body>
  <div class="page">
    <header class="bar">
      <h1>VESYL Print LCD</h1>
      <div class="meta">{w}&times;{h} · ~{fps} fps ·
        <a href="/snapshot.jpg">snapshot</a> ·
        <a href="/api/stats">api/stats</a>
      </div>
    </header>
    <div class="lcd-wrap">
      <img src="/stream.mjpg" alt="LCD stream" width="{w}" height="{h}"/>
    </div>

    <section class="claim-panel" id="claim-panel" hidden>
      <img class="claim-logo" src="/assets/logo.svg" alt="VESYL"/>
      <h2 id="claim-title">Claim this print node</h2>
      <p class="sub" id="claim-sub">
        Enter the <strong>8-character</strong> code from the warehouse admin.
        Paste is supported (dashes optional).
      </p>
      <form id="claim-form" autocomplete="off" spellcheck="false">
        <div class="code-row" id="code-row" role="group" aria-label="Claim code">
          <div class="code-group">
            <input class="code-box" type="text" inputmode="text" maxlength="1"
                   aria-label="Character 1" data-i="0"/>
            <input class="code-box" type="text" inputmode="text" maxlength="1"
                   aria-label="Character 2" data-i="1"/>
            <input class="code-box" type="text" inputmode="text" maxlength="1"
                   aria-label="Character 3" data-i="2"/>
            <input class="code-box" type="text" inputmode="text" maxlength="1"
                   aria-label="Character 4" data-i="3"/>
          </div>
          <div class="code-dash" aria-hidden="true"></div>
          <div class="code-group">
            <input class="code-box" type="text" inputmode="text" maxlength="1"
                   aria-label="Character 5" data-i="4"/>
            <input class="code-box" type="text" inputmode="text" maxlength="1"
                   aria-label="Character 6" data-i="5"/>
            <input class="code-box" type="text" inputmode="text" maxlength="1"
                   aria-label="Character 7" data-i="6"/>
            <input class="code-box" type="text" inputmode="text" maxlength="1"
                   aria-label="Character 8" data-i="7"/>
          </div>
        </div>
        <div class="claim-name">
          <label for="claim-name">Node name (optional)</label>
          <input id="claim-name" type="text" maxlength="80"
                 placeholder="e.g. Pack station 1" autocomplete="off"/>
        </div>
        <div class="claim-actions">
          <button type="submit" class="btn-claim" id="claim-btn">Claim device</button>
          <p class="claim-msg" id="claim-msg" role="status"></p>
        </div>
      </form>
      <p class="claim-host" id="claim-host"></p>
    </section>

    <div class="stats" id="stats">
      <p class="empty">Loading stats…</p>
    </div>
    <footer class="foot">
      <span id="refreshed">—</span>
      <span>Trusted LAN only · polls /api/stats every 2s</span>
    </footer>
  </div>
  <script>
    const root = document.getElementById('stats');
    const refreshed = document.getElementById('refreshed');
    const claimPanel = document.getElementById('claim-panel');
    const claimForm = document.getElementById('claim-form');
    const claimBtn = document.getElementById('claim-btn');
    const claimMsg = document.getElementById('claim-msg');
    const claimHost = document.getElementById('claim-host');
    const claimTitle = document.getElementById('claim-title');
    const claimSub = document.getElementById('claim-sub');
    const boxes = Array.from(document.querySelectorAll('.code-box'));
    let claiming = false;
    let printNotice = {{ text: '', kind: '' }};

    function esc(s) {{
      if (s == null || s === '') return '—';
      return String(s)
        .replace(/&/g,'&amp;').replace(/</g,'&lt;')
        .replace(/>/g,'&gt;').replace(/"/g,'&quot;');
    }}

    function levelClass(level) {{
      if (level === 'ok') return 'ok';
      if (level === 'warn') return 'warn';
      if (level === 'down') return 'down';
      return 'muted';
    }}

    function row(k, v, level) {{
      let val = esc(v);
      if (level) {{
        val = '<span class="dot ' + levelClass(level) + '"></span>' + val;
      }}
      return '<tr><th>' + esc(k) + '</th><td>' + val + '</td></tr>';
    }}

    function card(title, rowsHtml) {{
      return '<section class="card"><h2>' + esc(title) + '</h2>'
        + '<table><tbody>' + rowsHtml + '</tbody></table></section>';
    }}

    function pairingLevel(p) {{
      if (!p) return 'muted';
      if (p.pairing === 'revoked') return 'down';
      if (p.pairing !== 'paired') return 'warn';
      if (p.cloud === 'online') return 'ok';
      if (p.cloud === 'offline') return 'down';
      return 'warn';
    }}

    function printerLevel(status) {{
      const s = (status || '').toLowerCase();
      if (s === 'idle' || s === 'online') return 'ok';
      if (s === 'printing' || s === 'processing') return 'warn';
      if (s === 'stopped' || s === 'offline' || s === 'error') return 'down';
      return 'muted';
    }}

    function printerFormats(prn) {{
      if (prn && Array.isArray(prn.test_formats) && prn.test_formats.length) {{
        return prn.test_formats;
      }}
      return (prn && prn.supports_raw) ? ['pdf', 'zpl'] : ['pdf'];
    }}

    function testPrintHref(cups, fmt) {{
      return '/api/test-print?cups_name=' + encodeURIComponent(cups || '')
        + '&format=' + encodeURIComponent(fmt || 'pdf');
    }}

    function printerRow(prn) {{
      const name = (prn && (prn.display_name || prn.cups_name)) || 'Printer';
      const cups = (prn && prn.cups_name) || '';
      const label = (prn && (prn.status_message || prn.status)) || 'unknown';
      const formats = printerFormats(prn);
      let links = '';
      for (const fmt of formats) {{
        links += '<a class="test-print" href="' + testPrintHref(cups, fmt)
          + '" data-cups="' + esc(cups) + '" data-fmt="' + esc(fmt) + '">'
          + esc(String(fmt).toUpperCase()) + '</a>';
      }}
      return '<tr><th>' + esc(name) + '</th><td>'
        + '<span class="prn-status"><span class="dot '
        + levelClass(printerLevel(prn && prn.status)) + '"></span>'
        + esc(label) + '</span>'
        + (cups ? '<span class="test-links">' + links + '</span>' : '')
        + '</td></tr>';
    }}

    function needsClaim(data) {{
      const p = (data && data.pairing) || {{}};
      if (p.needs_claim === true) return true;
      if (p.needs_claim === false) return false;
      return p.pairing !== 'paired';
    }}

    function normalizePasted(raw) {{
      return String(raw || '')
        .toUpperCase()
        .replace(/[^A-Z0-9]/g, '')
        .slice(0, 8);
    }}

    function fillCode(chars) {{
      const clean = normalizePasted(chars);
      for (let i = 0; i < 8; i++) {{
        boxes[i].value = clean[i] || '';
      }}
      const next = Math.min(clean.length, 7);
      if (clean.length < 8) boxes[next].focus();
      else boxes[7].focus();
    }}

    function readCode() {{
      return boxes.map(b => (b.value || '').toUpperCase()).join('');
    }}

    function setClaimBusy(busy) {{
      claiming = busy;
      claimBtn.disabled = busy;
      boxes.forEach(b => {{ b.disabled = busy; }});
      const nameEl = document.getElementById('claim-name');
      if (nameEl) nameEl.disabled = busy;
      claimBtn.textContent = busy ? 'Claiming…' : 'Claim device';
    }}

    function showClaimMessage(text, kind) {{
      claimMsg.textContent = text || '';
      claimMsg.className = 'claim-msg' + (kind ? ' ' + kind : '');
    }}

    function updateClaimPanel(data) {{
      const show = needsClaim(data);
      claimPanel.hidden = !show;
      if (!show) return;
      const p = data.pairing || {{}};
      const s = data.system || {{}};
      if (p.pairing === 'revoked') {{
        claimTitle.textContent = 'Re-pair this print node';
        claimSub.innerHTML =
          'This device was <strong>revoked</strong>. Enter a new '
          + '<strong>8-character</strong> claim code from the warehouse admin.';
      }} else {{
        claimTitle.textContent = 'Claim this print node';
        claimSub.innerHTML =
          'Enter the <strong>8-character</strong> code from the warehouse admin. '
          + 'Paste is supported (dashes optional).';
      }}
      const bits = [];
      if (s.hostname) bits.push(s.hostname);
      if (s.primary_ip) bits.push(s.primary_ip);
      if (s.tailscale_ip && s.tailscale_ip !== 'n/a') bits.push('ts ' + s.tailscale_ip);
      claimHost.textContent = bits.join(' · ');
    }}

    boxes.forEach((box, i) => {{
      box.addEventListener('input', (e) => {{
        let v = (box.value || '').toUpperCase().replace(/[^A-Z0-9]/g, '');
        if (v.length > 1) {{
          // Mobile sometimes dumps paste into one box
          fillCode(readCode().slice(0, i) + v + readCode().slice(i + 1));
          return;
        }}
        box.value = v.slice(0, 1);
        if (box.value && i < 7) boxes[i + 1].focus();
      }});
      box.addEventListener('keydown', (e) => {{
        if (e.key === 'Backspace' && !box.value && i > 0) {{
          boxes[i - 1].focus();
          boxes[i - 1].value = '';
          e.preventDefault();
        }} else if (e.key === 'ArrowLeft' && i > 0) {{
          boxes[i - 1].focus();
          e.preventDefault();
        }} else if (e.key === 'ArrowRight' && i < 7) {{
          boxes[i + 1].focus();
          e.preventDefault();
        }}
      }});
      box.addEventListener('paste', (e) => {{
        e.preventDefault();
        const text = (e.clipboardData || window.clipboardData).getData('text') || '';
        fillCode(text);
      }});
      box.addEventListener('focus', () => {{ box.select(); }});
    }});

    // Paste anywhere on the form fills the code (ignores dashes)
    claimForm.addEventListener('paste', (e) => {{
      const t = e.target;
      if (t && t.id === 'claim-name') return;
      e.preventDefault();
      const text = (e.clipboardData || window.clipboardData).getData('text') || '';
      fillCode(text);
    }});

    claimForm.addEventListener('submit', async (e) => {{
      e.preventDefault();
      if (claiming) return;
      const code = readCode();
      if (code.length !== 8) {{
        showClaimMessage('Enter all 8 characters of the claim code.', 'err');
        boxes[Math.min(code.length, 7)].focus();
        return;
      }}
      const name = (document.getElementById('claim-name').value || '').trim();
      setClaimBusy(true);
      showClaimMessage('Contacting VESYL…', '');
      try {{
        const r = await fetch('/api/claim', {{
          method: 'POST',
          headers: {{ 'Content-Type': 'application/json' }},
          body: JSON.stringify({{ code, name: name || null }}),
        }});
        const body = await r.json().catch(() => ({{}}));
        if (!r.ok) {{
          throw new Error(body.error || body.message || ('HTTP ' + r.status));
        }}
        showClaimMessage(
          'Paired as ' + (body.name || body.node_id || 'node')
          + (body.warehouse_name ? ' · ' + body.warehouse_name : '')
          + '. Waiting for agent heartbeat…',
          'ok'
        );
        setTimeout(poll, 400);
        setTimeout(poll, 2000);
      }} catch (err) {{
        showClaimMessage(err.message || 'Claim failed', 'err');
        setClaimBusy(false);
        boxes[0].focus();
      }}
    }});

    function render(data) {{
      updateClaimPanel(data);
      if (!needsClaim(data) && claiming) {{
        setClaimBusy(false);
        showClaimMessage('', '');
      }}

      const p = data.pairing || {{}};
      const s = data.system || {{}};
      const j = data.jobs || {{}};
      const u = data.update || {{}};
      const printers = data.printers || [];

      let html = '';
      html += card('Pairing',
        row('Pairing', p.pairing, pairingLevel(p))
        + row('Cloud', p.cloud, p.cloud === 'online' ? 'ok' : (p.cloud === 'offline' ? 'down' : 'muted'))
        + row('Node', p.name || p.node_id)
        + row('Organization', p.organization_name)
        + row('Warehouse', p.warehouse_name)
        + row('Last heartbeat', p.last_heartbeat_age
              || p.last_heartbeat_at
              || null)
        + (p.last_heartbeat_at && p.last_heartbeat_age
              ? row('Heartbeat at', p.last_heartbeat_at) : '')
        + row('Agent version', p.agent_version)
        + (p.last_error ? row('Last error', p.last_error, 'down') : '')
      );

      html += card('System',
        row('Hostname', s.hostname)
        + row('LAN IP', s.primary_ip)
        + row('All IPs', (s.ip_addresses || []).join(', ') || null)
        + row('Tailscale', s.tailscale_ip)
        + row('CPU temp', s.cpu_temp)
        + row('Platform', s.platform)
        + row('Time', s.local_time)
      );

      let jobRows =
        row('Queued', j.queued)
        + row('Processed (markers)', j.processed)
        + row('Queue dir', j.queue_dir)
        + row('Processed dir', j.processed_dir);
      html += card('Jobs', jobRows);

      if (printers.length === 0) {{
        html += card('Printers', row('Status', 'No CUPS network queues', 'muted'));
      }} else {{
        let pr = '';
        for (const prn of printers) {{
          pr += printerRow(prn);
        }}
        if (printNotice.text) {{
          pr += '<tr><th></th><td class="print-notice '
            + (printNotice.kind || '') + '">' + esc(printNotice.text)
            + '</td></tr>';
        }}
        html += card('Printers', pr);
      }}

      let ota =
        row('Status', u.status || 'idle',
            (u.status && u.status !== 'idle') ? 'warn' : 'muted')
        + row('Current', u.current_version)
        + row('Target', u.target_version)
        + row('Channel', u.channel);
      if (u.last_error) ota += row('Last error', u.last_error, 'down');
      html += card('OTA / update', ota);

      const paths = data.paths || {{}};
      html += card('Paths',
        row('Config', paths.config_dir)
        + row('State', paths.state_dir)
        + row('Status file', paths.status_path)
        + row('Credentials', paths.credentials_present === true
              ? 'present' : (paths.credentials_present === false ? 'missing' : '—'))
        + row('API base', paths.api_base_url)
      );

      root.innerHTML = html;
      refreshed.textContent = 'Updated ' + (data.collected_at || new Date().toISOString());
    }}

    async function poll() {{
      try {{
        const r = await fetch('/api/stats', {{ cache: 'no-store' }});
        if (!r.ok) throw new Error('HTTP ' + r.status);
        render(await r.json());
      }} catch (e) {{
        root.innerHTML = '<section class="card"><h2>Stats</h2>'
          + '<p class="err">Failed to load stats: ' + esc(e.message) + '</p></section>';
      }}
    }}
    document.addEventListener('click', async (e) => {{
      const a = e.target && e.target.closest ? e.target.closest('a.test-print') : null;
      if (!a) return;
      e.preventDefault();
      if (a.dataset.busy === '1') return;
      const fmt = a.dataset.fmt || 'pdf';
      const cups = a.dataset.cups || '';
      const label = fmt.toUpperCase();
      a.dataset.busy = '1';
      a.classList.add('busy');
      printNotice = {{ text: 'Sending ' + label + '…', kind: '' }};
      const noticeEl = document.querySelector('.print-notice');
      if (noticeEl) {{
        noticeEl.textContent = printNotice.text;
        noticeEl.className = 'print-notice';
      }}
      try {{
        const r = await fetch('/api/test-print', {{
          method: 'POST',
          headers: {{ 'Content-Type': 'application/json', 'Accept': 'application/json' }},
          body: JSON.stringify({{ cups_name: cups, format: fmt }}),
        }});
        const body = await r.json().catch(() => ({{}}));
        if (!r.ok || body.ok === false) {{
          throw new Error(body.error || body.message || ('HTTP ' + r.status));
        }}
        const dest = body.display_name || body.cups_name || cups;
        printNotice = {{
          text: label + ' sent to ' + dest + (body.result ? ' (' + body.result + ')' : ''),
          kind: 'ok',
        }};
      }} catch (err) {{
        printNotice = {{ text: 'Test print failed: ' + (err.message || err), kind: 'err' }};
      }}
      a.dataset.busy = '0';
      a.classList.remove('busy');
      const after = document.querySelector('.print-notice');
      if (after) {{
        after.textContent = printNotice.text;
        after.className = 'print-notice ' + (printNotice.kind || '');
      }}
    }});

    poll();
    setInterval(poll, 2000);
  </script>
</body>
</html>
"""


class FrameSource:
    """Thread-safe latest JPEG. Fed by ``publish()`` or a capture loop."""

    def __init__(
        self,
        *,
        scale: float = DEFAULT_SCALE,
        quality: int = DEFAULT_QUALITY,
        native_size: tuple[int, int] | None = None,
    ):
        self.scale = max(scale, 0.25)
        self.quality = max(40, min(quality, 95))
        self._native = native_size or (480, 320)
        self._jpeg = b""
        self._lock = threading.Lock()
        self._error: str | None = None

    @property
    def size(self) -> tuple[int, int]:
        w, h = self._native
        if self.scale != 1.0:
            return (max(1, round(w * self.scale)), max(1, round(h * self.scale)))
        return (w, h)

    def set_native_size(self, size: tuple[int, int]) -> None:
        self._native = size

    def publish(self, image: Image.Image) -> None:
        """Encode ``image`` to JPEG for stream clients."""
        try:
            self._native = image.size
            img = image
            if img.mode != "RGB":
                img = img.convert("RGB")
            if self.scale != 1.0:
                tw, th = self.size
                img = img.resize((tw, th), Image.NEAREST)
            buf = io.BytesIO()
            img.save(buf, format="JPEG", quality=self.quality, optimize=True)
            data = buf.getvalue()
            with self._lock:
                self._jpeg = data
                self._error = None
        except Exception as e:
            with self._lock:
                self._error = str(e)

    def latest_jpeg(self) -> bytes:
        with self._lock:
            return self._jpeg

    def error(self) -> str | None:
        with self._lock:
            return self._error


class FrameGrabber:
    """Background thread that captures /dev/fb1 into a FrameSource (standalone)."""

    def __init__(
        self,
        device: str,
        source: FrameSource,
        fps: float,
    ):
        from framebuffer import Framebuffer

        self.fb = Framebuffer(device)
        source.set_native_size(self.fb.size)
        self.source = source
        self.interval = 1.0 / max(fps, 0.2)
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None

    def start(self) -> None:
        self._thread = threading.Thread(
            target=self._loop, name="lcd-grabber", daemon=True
        )
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        if self._thread and self._thread.is_alive():
            self._thread.join(timeout=2.0)

    def _loop(self) -> None:
        while not self._stop.is_set():
            t0 = time.monotonic()
            try:
                self.source.publish(self.fb.capture())
            except Exception as e:
                with self.source._lock:
                    self.source._error = str(e)
            elapsed = time.monotonic() - t0
            self._stop.wait(max(0.0, self.interval - elapsed))


def _count_json_files(directory: Path | None) -> int:
    if directory is None or not directory.is_dir():
        return 0
    try:
        return sum(1 for f in directory.iterdir() if f.is_file() and f.suffix == ".json")
    except OSError:
        return 0


def normalize_claim_code(raw: str | None) -> str:
    """Uppercase alphanumerics only — strips dashes, spaces, and other punctuation."""
    return "".join(c for c in (raw or "").upper() if c.isalnum())


class ClaimError(Exception):
    """User-facing claim failure (safe to return in HTTP JSON)."""

    def __init__(self, message: str, *, status: int = 400):
        super().__init__(message)
        self.message = message
        self.status = status


class TestPrintError(Exception):
    """User-facing test-print failure (safe to return in HTTP JSON)."""

    def __init__(self, message: str, *, status: int = 400):
        super().__init__(message)
        self.message = message
        self.status = status


def _printer_test_formats(item: dict[str, Any]) -> list[str]:
    from display_status import test_print_formats

    return list(test_print_formats(bool(item.get("supports_raw"))))


def test_print_href(cups_name: str, fmt: str) -> str:
    return (
        f"/api/test-print?cups_name={quote(str(cups_name or ''), safe='')}"
        f"&format={quote(str(fmt or 'pdf'), safe='')}"
    )


def run_test_print(
    cups_name: str,
    fmt: str,
    *,
    inventory: list[dict[str, Any]] | None = None,
    submit: Callable[..., str] | None = None,
) -> dict[str, Any]:
    """Send the Road Runner sample label to a configured CUPS queue."""
    from display_status import test_print_formats

    cups = (cups_name or "").strip()
    kind = (fmt or "pdf").strip().lower()
    if not cups:
        raise TestPrintError("missing cups_name", status=400)
    if kind not in ("pdf", "zpl"):
        raise TestPrintError(f"unsupported format: {fmt}", status=400)

    items = inventory
    if items is None:
        try:
            import printers

            items = list(printers.inventory_payload())
        except Exception as e:
            raise TestPrintError(
                f"printer inventory unavailable: {e}", status=503
            ) from e

    match: dict[str, Any] | None = None
    for item in items:
        name = str(item.get("cups_name") or "")
        display = str(item.get("display_name") or "")
        if name == cups or display == cups:
            match = item
            break
    if match is None:
        raise TestPrintError(f"unknown printer: {cups}", status=404)

    queue = str(match.get("cups_name") or cups)
    allowed = test_print_formats(bool(match.get("supports_raw")))
    if kind not in allowed:
        raise TestPrintError(
            f"{kind.upper()} is only available on raw/Zebra queues",
            status=400,
        )

    if submit is None:
        from test_label import submit_test_label

        submit = submit_test_label
    try:
        result = submit(queue, kind, wait_cups=False)
    except TestPrintError:
        raise
    except Exception as e:
        msg = getattr(e, "message", None) or str(e)
        raise TestPrintError(msg, status=500) from e

    return {
        "ok": True,
        "cups_name": queue,
        "display_name": match.get("display_name") or queue,
        "format": kind,
        "result": result,
    }


def claim_device(
    code: str,
    *,
    name: str | None = None,
    cfg: Any = None,
) -> dict[str, Any]:
    """Pair this node with an 8-char claim code. Never returns device_token."""
    import auth
    import statusio
    import sysinfo
    from cloud import CloudClient, CloudError
    from config import AGENT_VERSION, default_platform, load_config, write_default_config

    if cfg is None:
        cfg = load_config()
    cfg.ensure_dirs()
    write_default_config(cfg.config_path)

    clean = normalize_claim_code(code)
    if len(clean) != _CLAIM_CODE_LEN:
        raise ClaimError(
            f"Claim code must be {_CLAIM_CODE_LEN} characters "
            f"(got {len(clean)} after removing dashes/spaces)",
            status=400,
        )

    # Refuse if already paired with local credentials.
    if auth.load_credentials(cfg.credentials_path) is not None:
        st = statusio.read_status(cfg.status_path)
        if st and st.pairing == "paired":
            raise ClaimError("This node is already claimed", status=409)

    client = CloudClient(cfg.api_base_url)
    try:
        data = client.claim(
            clean,
            hostname=sysinfo.hostname(),
            agent_version=AGENT_VERSION,
            platform=default_platform(),
            name=(name.strip() if name else None) or None,
        )
    except CloudError as e:
        raise ClaimError(e.message or "Claim failed", status=e.status or 502) from e

    token = data.get("device_token")
    if not token:
        raise ClaimError("Claim response missing device_token", status=502)

    creds = auth.credentials_from_pair_response(data)
    auth.save_credentials(cfg.credentials_path, creds)
    statusio.write_status(
        cfg.status_path,
        statusio.AgentStatus(
            pairing="paired",
            cloud="offline",
            node_id=creds.node_id,
            name=creds.name,
            organization_name=creds.organization_name,
            warehouse_name=creds.warehouse_label(),
            agent_version=AGENT_VERSION,
        ),
    )
    log.info(
        "stream claim ok node_id=%s org=%s warehouse=%s",
        creds.node_id,
        creds.organization_name,
        creds.warehouse_label(),
    )
    return {
        "ok": True,
        "node_id": creds.node_id,
        "name": creds.name,
        "organization_name": creds.organization_name,
        "warehouse_name": creds.warehouse_label(),
    }


def collect_stats(
    *,
    status_path: Path | str | None = None,
    update_status_path: Path | str | None = None,
    queue_dir: Path | str | None = None,
    processed_dir: Path | str | None = None,
    credentials_path: Path | str | None = None,
    config_dir: Path | str | None = None,
    state_dir: Path | str | None = None,
    api_base_url: str | None = None,
    include_printers: bool = True,
) -> dict[str, Any]:
    """Gather pairing / system / print / OTA snapshot for the stream page."""
    # Lazy imports keep standalone stream import light when modules fail.
    import statusio
    import sysinfo
    import update as update_mod
    from config import AGENT_VERSION, default_platform
    from display_status import format_agent_version, heartbeat_age_label

    st = None
    if status_path:
        st = statusio.read_status(Path(status_path))

    pairing_state = st.pairing if st else "unpaired"
    pairing: dict[str, Any] = {
        "pairing": pairing_state,
        "needs_claim": pairing_state != "paired",
        "cloud": st.cloud if st else "unknown",
        "node_id": st.node_id if st else None,
        "name": st.name if st else None,
        "organization_name": st.organization_name if st else None,
        "warehouse_name": st.warehouse_name if st else None,
        "last_heartbeat_at": st.last_heartbeat_at if st else None,
        "last_heartbeat_age": heartbeat_age_label(
            st.last_heartbeat_at if st else None
        ),
        "last_error": st.last_error if st else None,
        "agent_version": format_agent_version(
            (st.agent_version if st and st.agent_version else None) or AGENT_VERSION
        ),
        "status_updated_at": st.updated_at if st else None,
    }

    system = {
        "hostname": sysinfo.hostname(),
        "primary_ip": sysinfo.primary_ip(),
        "ip_addresses": sysinfo.ip_addresses(),
        "tailscale_ip": sysinfo.tailscale_ip(),
        "cpu_temp": sysinfo.cpu_temp_c(),
        "platform": default_platform(),
        "local_time": sysinfo.now().strftime("%Y-%m-%d %H:%M:%S"),
    }

    qdir = Path(queue_dir) if queue_dir else None
    pdir = Path(processed_dir) if processed_dir else None
    jobs = {
        "queued": _count_json_files(qdir),
        "processed": _count_json_files(pdir),
        "queue_dir": str(qdir) if qdir else None,
        "processed_dir": str(pdir) if pdir else None,
    }

    printers_list: list[dict[str, Any]] = []
    if include_printers:
        try:
            import printers

            for item in printers.inventory_payload():
                cups = item.get("cups_name")
                formats = _printer_test_formats(item)
                printers_list.append(
                    {
                        "cups_name": cups,
                        "display_name": item.get("display_name"),
                        "status": item.get("status"),
                        "status_message": item.get("status_message"),
                        "uri": item.get("uri"),
                        "supports_raw": bool(item.get("supports_raw")),
                        "test_formats": formats,
                        "test_print": {
                            fmt: test_print_href(str(cups or ""), fmt)
                            for fmt in formats
                        },
                    }
                )
        except Exception as e:
            log.debug("printer inventory for stream stats failed: %s", e)

    ust = None
    if update_status_path:
        ust = update_mod.read_update_status(Path(update_status_path))
    update_info: dict[str, Any] = {
        "status": ust.status if ust else "idle",
        "current_version": (
            (ust.current_version if ust and ust.current_version else None)
            or format_agent_version(AGENT_VERSION)
        ),
        "target_version": ust.target_version if ust else None,
        "channel": ust.channel if ust else None,
        "last_error": ust.last_error if ust else None,
        "last_checked_at": ust.last_checked_at if ust else None,
        "previous_version": ust.previous_version if ust else None,
    }

    creds_present: bool | None = None
    if credentials_path:
        creds_present = Path(credentials_path).is_file()

    paths = {
        "config_dir": str(config_dir) if config_dir else None,
        "state_dir": str(state_dir) if state_dir else None,
        "status_path": str(status_path) if status_path else None,
        "credentials_path": str(credentials_path) if credentials_path else None,
        "credentials_present": creds_present,
        "api_base_url": api_base_url,
    }

    return {
        "collected_at": datetime.now(timezone.utc)
        .replace(microsecond=0)
        .isoformat(),
        "pairing": pairing,
        "system": system,
        "jobs": jobs,
        "printers": printers_list,
        "update": update_info,
        "paths": paths,
    }


class StatsProvider:
    """Cached stats collector for the HTTP handler."""

    def __init__(
        self,
        collector: Callable[[], dict[str, Any]] | None = None,
        *,
        cache_s: float = _STATS_CACHE_S,
    ):
        self._collector = collector
        self._cache_s = cache_s
        self._lock = threading.Lock()
        self._cached: dict[str, Any] | None = None
        self._cached_at = 0.0

    def get(self) -> dict[str, Any]:
        if self._collector is None:
            return {
                "collected_at": datetime.now(timezone.utc)
                .replace(microsecond=0)
                .isoformat(),
                "pairing": {},
                "system": {},
                "jobs": {},
                "printers": [],
                "update": {},
                "paths": {},
                "error": "stats not configured",
            }
        now = time.monotonic()
        with self._lock:
            if self._cached is not None and (now - self._cached_at) < self._cache_s:
                return self._cached
        try:
            data = self._collector()
        except Exception as e:
            log.exception("stats collect failed")
            data = {
                "collected_at": datetime.now(timezone.utc)
                .replace(microsecond=0)
                .isoformat(),
                "error": str(e),
                "pairing": {},
                "system": {},
                "jobs": {},
                "printers": [],
                "update": {},
                "paths": {},
            }
        with self._lock:
            self._cached = data
            self._cached_at = time.monotonic()
            return data

    def invalidate(self) -> None:
        with self._lock:
            self._cached = None
            self._cached_at = 0.0


def make_handler(
    source: FrameSource,
    fps: float,
    stats: StatsProvider | None = None,
    *,
    claim_fn: Callable[..., dict[str, Any]] | None = None,
) -> type[BaseHTTPRequestHandler]:
    stats_provider = stats or StatsProvider(None)
    do_claim = claim_fn or claim_device

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, fmt: str, *args: Any) -> None:
            sys.stderr.write("%s - %s\n" % (self.address_string(), fmt % args))

        def do_GET(self) -> None:  # noqa: N802
            path = urlparse(self.path).path
            if path in ("/", "/index.html"):
                self._serve_html()
            elif path in ("/stream.mjpg", "/stream.mjpeg", "/mjpeg"):
                self._serve_mjpeg()
            elif path in ("/snapshot.jpg", "/snapshot.jpeg", "/snap.jpg"):
                self._serve_snapshot()
            elif path in ("/api/stats", "/stats.json"):
                self._serve_stats()
            elif path in ("/api/test-print", "/test-print"):
                self._serve_test_print()
            elif path in ("/assets/logo.svg", "/logo.svg"):
                self._serve_asset("logo.svg", "image/svg+xml")
            elif path in ("/assets/logo.png", "/logo.png"):
                self._serve_asset("logo.png", "image/png")
            elif path == "/health":
                self._serve_health()
            else:
                self.send_error(404, "not found")

        def do_POST(self) -> None:  # noqa: N802
            path = urlparse(self.path).path
            if path in ("/api/claim", "/claim"):
                self._serve_claim()
            elif path in ("/api/test-print", "/test-print"):
                self._serve_test_print()
            else:
                self.send_error(404, "not found")

        def _read_json_body(self, max_bytes: int = 4096) -> dict[str, Any]:
            try:
                length = int(self.headers.get("Content-Length") or "0")
            except ValueError:
                length = 0
            if length < 0 or length > max_bytes:
                raise ClaimError("Request body too large", status=413)
            raw = self.rfile.read(length) if length else b""
            if not raw:
                return {}
            try:
                data = json.loads(raw.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError) as e:
                raise ClaimError("Invalid JSON body", status=400) from e
            if not isinstance(data, dict):
                raise ClaimError("JSON body must be an object", status=400)
            return data

        def _json_response(self, status: int, payload: dict[str, Any]) -> None:
            body = json.dumps(payload).encode("utf-8") + b"\n"
            self.send_response(status)
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(body)

        def _serve_test_print(self) -> None:
            parsed = urlparse(self.path)
            qs = parse_qs(parsed.query)
            cups = (qs.get("cups_name") or qs.get("printer") or [None])[0]
            fmt = (qs.get("format") or qs.get("fmt") or [None])[0]
            if self.command == "POST":
                try:
                    data = self._read_json_body()
                except ClaimError as e:
                    self._json_response(e.status, {"ok": False, "error": e.message})
                    return
                cups = data.get("cups_name") or data.get("printer") or cups
                fmt = data.get("format") or data.get("fmt") or fmt
            try:
                result = run_test_print(str(cups or ""), str(fmt or "pdf"))
                self._json_response(200, result)
            except TestPrintError as e:
                self._json_response(e.status, {"ok": False, "error": e.message})
            except Exception:
                log.exception("test-print endpoint failed")
                self._json_response(
                    500, {"ok": False, "error": "Internal test-print error"}
                )

        def _serve_claim(self) -> None:
            try:
                data = self._read_json_body()
                code = data.get("code")
                if code is None:
                    code = data.get("claim_code")
                name = data.get("name")
                if name is not None:
                    name = str(name)
                # Optional: accept dashed code as single string or joined digits
                if isinstance(code, list):
                    code = "".join(str(c) for c in code)
                result = do_claim(str(code or ""), name=name)
                stats_provider.invalidate()
                self._json_response(200, result)
            except ClaimError as e:
                self._json_response(e.status, {"ok": False, "error": e.message})
            except Exception:
                log.exception("claim endpoint failed")
                self._json_response(
                    500, {"ok": False, "error": "Internal claim error"}
                )

        def _serve_asset(self, name: str, content_type: str) -> None:
            path = _ASSETS_DIR / name
            if not path.is_file():
                self.send_error(404, "not found")
                return
            try:
                data = path.read_bytes()
            except OSError:
                self.send_error(500, "read error")
                return
            self.send_response(200)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Cache-Control", "public, max-age=3600")
            self.end_headers()
            self.wfile.write(data)

        def _serve_html(self) -> None:
            w, h = source.size
            body = HTML_PAGE.format(
                w=w, h=h, fps=fps, css_max=max(w, 480) * 2
            ).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(body)

        def _serve_stats(self) -> None:
            data = stats_provider.get()
            body = json.dumps(data, indent=2).encode("utf-8") + b"\n"
            self.send_response(200)
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("Access-Control-Allow-Origin", "*")
            self.end_headers()
            self.wfile.write(body)

        def _serve_snapshot(self) -> None:
            data = source.latest_jpeg()
            if not data:
                self.send_error(503, source.error() or "no frame yet")
                return
            self.send_response(200)
            self.send_header("Content-Type", "image/jpeg")
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(data)

        def _serve_health(self) -> None:
            body = b"ok\n" if source.latest_jpeg() else b"warming\n"
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def _serve_mjpeg(self) -> None:
            self.send_response(200)
            self.send_header(
                "Content-Type",
                f"multipart/x-mixed-replace; boundary={BOUNDARY.decode()}",
            )
            self.send_header("Cache-Control", "no-store, no-cache, must-revalidate")
            self.send_header("Pragma", "no-cache")
            self.send_header("Connection", "close")
            self.end_headers()

            interval = 1.0 / max(fps, 0.2)
            try:
                while True:
                    data = source.latest_jpeg()
                    if data:
                        header = (
                            b"--" + BOUNDARY + b"\r\n"
                            b"Content-Type: image/jpeg\r\n"
                            b"Content-Length: " + str(len(data)).encode() + b"\r\n"
                            b"\r\n"
                        )
                        self.wfile.write(header)
                        self.wfile.write(data)
                        self.wfile.write(b"\r\n")
                        self.wfile.flush()
                    time.sleep(interval)
            except (BrokenPipeError, ConnectionResetError, OSError):
                return

    return Handler


class LcdStreamServer:
    """Background HTTP MJPEG server fed by ``publish(image)`` each display frame."""

    def __init__(
        self,
        *,
        host: str = "0.0.0.0",
        port: int = DEFAULT_PORT,
        fps: float = DEFAULT_FPS,
        scale: float = DEFAULT_SCALE,
        quality: int = DEFAULT_QUALITY,
        native_size: tuple[int, int] | None = None,
        stats_collector: Callable[[], dict[str, Any]] | None = None,
    ):
        self.host = host
        self.port = port
        self.fps = fps
        self.source = FrameSource(
            scale=scale, quality=quality, native_size=native_size
        )
        self.stats = StatsProvider(stats_collector)
        self._server: ThreadingHTTPServer | None = None
        self._thread: threading.Thread | None = None

    def start(self) -> None:
        if self._thread and self._thread.is_alive():
            return
        handler = make_handler(self.source, self.fps, self.stats)
        try:
            self._server = ThreadingHTTPServer((self.host, self.port), handler)
        except OSError as e:
            log.warning("LCD stream not started on %s:%s: %s", self.host, self.port, e)
            self._server = None
            return
        # daemon thread so display service can exit on SIGTERM without hang
        self._thread = threading.Thread(
            target=self._serve, name="lcd-stream", daemon=True
        )
        self._thread.start()
        log.info(
            "LCD stream http://0.0.0.0:%s/ (scale=%s fps~%s)",
            self.port,
            self.source.scale,
            self.fps,
        )

    def _serve(self) -> None:
        assert self._server is not None
        try:
            self._server.serve_forever(poll_interval=0.5)
        except Exception:
            log.exception("LCD stream server stopped with error")

    def publish(self, image: Image.Image) -> None:
        self.source.publish(image)

    def stop(self) -> None:
        if self._server is not None:
            try:
                self._server.shutdown()
            except Exception:
                pass
            try:
                self._server.server_close()
            except Exception:
                pass
            self._server = None
        if self._thread and self._thread.is_alive():
            self._thread.join(timeout=2.0)
        self._thread = None


def _default_stats_collector() -> Callable[[], dict[str, Any]]:
    from config import load_config

    cfg = load_config()

    def _collect() -> dict[str, Any]:
        return collect_stats(
            status_path=cfg.status_path,
            update_status_path=cfg.update_status_path,
            queue_dir=cfg.queue_dir,
            processed_dir=cfg.processed_dir,
            credentials_path=cfg.credentials_path,
            config_dir=cfg.config_dir,
            state_dir=cfg.state_dir,
            api_base_url=cfg.api_base_url,
        )

    return _collect


def main() -> None:
    ap = argparse.ArgumentParser(description="MJPEG stream of the vesyl-print LCD")
    ap.add_argument("--device", default="/dev/fb1", help="framebuffer device")
    ap.add_argument("--host", default="0.0.0.0", help="bind address")
    ap.add_argument("--port", type=int, default=DEFAULT_PORT, help="HTTP port")
    ap.add_argument(
        "--fps", type=float, default=DEFAULT_FPS, help="capture / stream rate"
    )
    ap.add_argument(
        "--scale",
        type=float,
        default=DEFAULT_SCALE,
        help="upscale factor (nearest-neighbor)",
    )
    ap.add_argument(
        "--quality", type=int, default=DEFAULT_QUALITY, help="JPEG quality 40-95"
    )
    args = ap.parse_args()

    try:
        source = FrameSource(scale=args.scale, quality=args.quality)
        grabber = FrameGrabber(args.device, source, args.fps)
    except (OSError, RuntimeError) as e:
        print(f"Cannot open {args.device}: {e}", file=sys.stderr)
        print("Is the display service running? Are you in group 'video'?", file=sys.stderr)
        raise SystemExit(1) from e

    grabber.start()
    for _ in range(50):
        if source.latest_jpeg():
            break
        time.sleep(0.05)

    stats = StatsProvider(_default_stats_collector())
    handler = make_handler(source, args.fps, stats)
    server = ThreadingHTTPServer((args.host, args.port), handler)

    def _shutdown(*_args: object) -> None:
        print("\nStopping…", flush=True)
        threading.Thread(target=server.shutdown, daemon=True).start()

    signal.signal(signal.SIGINT, _shutdown)
    signal.signal(signal.SIGTERM, _shutdown)

    w, h = source.size
    print(
        f"Streaming {args.device} ({grabber.fb.width}x{grabber.fb.height}"
        f" → {w}x{h} @ ~{args.fps} fps)",
        flush=True,
    )
    print(f"  Open  http://<pi-ip>:{args.port}/", flush=True)
    print(f"  Snap  http://<pi-ip>:{args.port}/snapshot.jpg", flush=True)
    print(f"  Stats http://<pi-ip>:{args.port}/api/stats", flush=True)
    try:
        server.serve_forever(poll_interval=0.5)
    finally:
        grabber.stop()
        server.server_close()


if __name__ == "__main__":
    main()
