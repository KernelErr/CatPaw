#!/usr/bin/env node
// A token baseline for the task set: runs the tasks in tests/tasks/
// through Playwright MCP, live, and counts the bytes of what the agent
// receives, for `cargo xtask tasks report --baseline`.
//
// Playwright MCP keeps the page snapshot in a file and links it from a
// result whenever the page changed; the agent reads that snapshot to see
// the page, so its bytes count, as one more call. A step whose target is
// `role "name"` takes the ref from the last snapshot; `css:` and `text:`
// targets pass as selectors (text matched exactly). A `read` step takes
// no call, since the last snapshot already shows what it reads, and
// neither does the user's approval of a confirmation or the call CatPaw
// repeats once it is given. The checks run after the steps and are not
// counted. Each task runs three times, and the median run counts.
//
//   npm install --prefix <dir> @playwright/mcp@<version>
//   node tools/baseline/playwright-mcp.mjs <dir>/node_modules/.bin/playwright-mcp [--local] [task ids]
//
// BASELINE_VERBOSE=1 prints each call and snapshot to stderr.
//
// Writes tools/baseline/playwright-mcp.json.

import { spawn } from 'node:child_process';
import {
  copyFileSync,
  existsSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  realpathSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join, resolve } from 'node:path';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const argv = process.argv.slice(2);
// --local measures the tasks in tests/tasks/local/ (content sites, kept
// out of the repository) and keeps the numbers there too.
const local = argv.includes('--local');
const [bin, ...only] = argv.filter((a) => a !== '--local');
const verbose = process.env.BASELINE_VERBOSE === '1';
if (!bin) {
  console.error('usage: playwright-mcp.mjs <playwright-mcp binary> [task ids]');
  process.exit(2);
}

class Client {
  constructor(cwd) {
    this.cwd = cwd;
    this.child = spawn(bin, ['--headless', '--isolated'], {
      cwd,
      stdio: ['pipe', 'pipe', 'ignore'],
    });
    this.last = 0;
    this.waiting = new Map();
    createInterface({ input: this.child.stdout }).on('line', (line) => {
      let message;
      try {
        message = JSON.parse(line);
      } catch {
        return;
      }
      const done = this.waiting.get(message.id);
      if (done) {
        this.waiting.delete(message.id);
        done(message);
      }
    });
  }

  request(method, params) {
    const id = ++this.last;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`${method}: no answer in 120 s`)), 120_000);
      this.waiting.set(id, (message) => {
        clearTimeout(timer);
        resolve(message);
      });
      this.child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
    });
  }

  async start() {
    const reply = await this.request('initialize', {
      protocolVersion: '2025-06-18',
      capabilities: {},
      clientInfo: { name: 'catpaw-baseline', version: '0' },
    });
    this.child.stdin.write(JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }) + '\n');
    return reply.result.serverInfo;
  }

  async tool(name, args) {
    const reply = await this.request('tools/call', { name, arguments: args });
    if (reply.error) throw new Error(`${name}: ${JSON.stringify(reply.error)}`);
    const text = reply.result.content
      .filter((c) => c.type === 'text')
      .map((c) => c.text)
      .join('\n');
    return { text, isError: reply.result.isError === true };
  }

  close() {
    this.child.kill();
  }
}

/// One task, as an agent on Playwright MCP would take it.
async function run(task) {
  const cwd = realpathSync(mkdtempSync(join(tmpdir(), 'pw-baseline-')));
  const client = new Client(cwd);
  const result = { calls: 0, snapshot_reads: 0, bytes: 0, first_view_bytes: 0, passed: false };
  let snapshot = '';
  try {
    await client.start();
    const call = async (name, args) => {
      const { text, isError } = await client.tool(name, args);
      result.calls += 1;
      result.bytes += Buffer.byteLength(text);
      if (verbose) console.error(`> ${name} ${JSON.stringify(args)}\n${text}\n`);
      if (isError) throw new Error(`${name} ${JSON.stringify(args)}:\n${text}`);
      const link = text.match(/\[Snapshot\]\(([^)]+)\)/);
      if (link) {
        snapshot = readFileSync(resolve(cwd, link[1]), 'utf8');
        result.calls += 1;
        result.snapshot_reads += 1;
        result.bytes += Buffer.byteLength(snapshot);
        if (verbose) console.error(`> (snapshot)\n${snapshot}\n`);
      } else {
        const inline = text.match(/```yaml\n([\s\S]*?)```/);
        if (inline) snapshot = inline[1];
      }
      return text;
    };
    const target = (spec) => {
      if (spec.startsWith('css:')) return spec.slice(4);
      if (spec.startsWith('text:')) return `text=${JSON.stringify(spec.slice(5))}`;
      const m = spec.match(/^([a-z]+) "(.*)"$/);
      if (!m) throw new Error(`no mapping for the target ${spec}`);
      // Names compare by their letters and digits: Playwright counts icon
      // glyphs from CSS content in a name, CatPaw does not.
      const plain = (name) => name.replace(/[^\p{L}\p{N}]+/gu, ' ').trim().toLowerCase();
      for (const line of snapshot.split('\n')) {
        const shown = line.trim().match(/^- ([a-z]+) ("(?:[^"\\]|\\.)*")(.*)$/);
        const ref = shown?.[3].match(/\[ref=([^\]]+)\]/);
        if (ref && shown[1] === m[1] && plain(JSON.parse(shown[2])) === plain(m[2])) return ref[1];
      }
      throw new Error(`${spec} is not in the snapshot`);
    };
    const element = (args) => ({ element: args.target, target: target(args.target) });

    await call('browser_navigate', { url: task.start_url });
    result.first_view_bytes = result.bytes;
    for (const step of task.steps) {
      const { tool, args } = step;
      // Playwright MCP asks no one: the user's approval and the call
      // CatPaw repeats with it take nothing there.
      if (step.approve !== undefined || step.decline !== undefined || args?.confirmation !== undefined) {
        continue;
      }
      switch (tool) {
        case 'navigate':
          await call('browser_navigate', { url: args.url });
          break;
        case 'click':
          await call('browser_click', element(args));
          if (args.promptText !== undefined || args.dialog !== undefined) {
            const answer = { accept: args.dialog !== 'dismiss' };
            if (args.promptText !== undefined) answer.promptText = args.promptText;
            await call('browser_handle_dialog', answer);
          }
          break;
        case 'type':
          await call('browser_type', { ...element(args), text: args.text, ...(args.submit ? { submit: true } : {}) });
          break;
        case 'select':
          await call('browser_select_option', { ...element(args), values: [].concat(args.option) });
          break;
        case 'press':
          await call('browser_press_key', { key: args.key });
          break;
        case 'act':
          if (args.kind === 'scroll') {
            await call('browser_evaluate', { function: `() => window.scrollBy(${args.dx ?? 0}, ${args.dy ?? 0})` });
          } else if (args.kind === 'check' || args.kind === 'uncheck') {
            await call('browser_click', element(args));
          } else if (args.kind === 'upload') {
            // Clicking the input opens the file chooser, which takes the
            // files; Playwright MCP reads files only under its working
            // directory, so they are copied there.
            await call('browser_click', element(args));
            const paths = args.files.map((f) => {
              const copy = join(cwd, basename(f));
              copyFileSync(join(tasksDir, task.id, f), copy);
              return copy;
            });
            await call('browser_file_upload', { paths });
          } else {
            throw new Error(`act ${args.kind}: no mapping`);
          }
          break;
        case 'wait':
          await call('browser_wait_for', args.for === 'text' ? { text: args.text } : { time: 2 });
          break;
        case 'tabs':
          if (args.op !== 'switch') throw new Error(`tabs ${args.op}: no mapping`);
          await call('browser_tabs', { action: 'select', index: Number(args.tab.slice(1)) - 1 });
          break;
        case 'evaluate':
          await call('browser_evaluate', { function: `() => (${args.script})` });
          break;
        case 'snapshot':
          await call('browser_snapshot', {});
          break;
        case 'read':
          break;
        default:
          throw new Error(`${tool}: no mapping`);
      }
    }

    const evaluate = async (script) => {
      const { text } = await client.tool('browser_evaluate', { function: `() => String(${script})` });
      const m = text.match(/### Result\n([\s\S]*?)(\n###|$)/);
      const raw = (m ? m[1] : text).trim();
      try {
        return JSON.parse(raw);
      } catch {
        return raw;
      }
    };
    let passed = true;
    for (const check of task.success) {
      if (check.url_contains !== undefined) {
        passed &&= (await evaluate('location.href')).includes(check.url_contains);
      } else if (check.text_contains !== undefined) {
        passed &&= (await evaluate('document.body.innerText')).includes(check.text_contains);
      } else if (check.snapshot_contains !== undefined) {
        passed &&= snapshot.includes(check.snapshot_contains.replace(/^text: /, ''));
      } else if (check.eval !== undefined) {
        passed &&= (await evaluate(check.eval)) === check.equals;
      }
    }
    result.passed = passed;
  } catch (e) {
    result.error = String(e.message ?? e).split('\n')[0];
  } finally {
    client.close();
    rmSync(cwd, { recursive: true, force: true });
  }
  return result;
}

const tasksDir = join(root, local ? 'tests/tasks/local' : 'tests/tasks');
const ids = readdirSync(tasksDir)
  .filter((id) => existsSync(join(tasksDir, id, 'task.json')))
  .filter((id) => only.length === 0 || only.includes(id))
  .sort();

const probe = new Client(realpathSync(mkdtempSync(join(tmpdir(), 'pw-baseline-'))));
const server = await probe.start();
const tools = await probe.request('tools/list', {});
probe.close();
rmSync(probe.cwd, { recursive: true, force: true });

/// The npm package the binary comes from, with its version.
function packageOf(path) {
  for (let dir = dirname(realpathSync(path)); dir !== dirname(dir); dir = dirname(dir)) {
    const manifest = join(dir, 'package.json');
    if (existsSync(manifest)) {
      const { name, version } = JSON.parse(readFileSync(manifest, 'utf8'));
      return `${name} ${version}`;
    }
  }
  return `${server.name} ${server.version}`;
}

const path = local
  ? join(tasksDir, 'playwright-mcp.json')
  : join(root, 'tools/baseline/playwright-mcp.json');
// Measuring some tasks again keeps what was measured of the others.
const kept = only.length > 0 && existsSync(path) ? JSON.parse(readFileSync(path, 'utf8')).tasks : {};
const out = {
  server: packageOf(bin),
  browser: 'Chrome, headless',
  date: new Date().toLocaleDateString('sv-SE'),
  tools_list_bytes: Buffer.byteLength(JSON.stringify(tools.result.tools)),
  tasks: kept,
};
for (const id of ids) {
  const task = JSON.parse(readFileSync(join(tasksDir, id, 'task.json'), 'utf8'));
  // Live pages vary (console noise, slow loads): the median of three
  // passing runs counts, out of six tries at most.
  const passing = [];
  let result;
  for (let attempt = 0; attempt < 6 && passing.length < 3; attempt++) {
    result = await run(task);
    if (result.passed) passing.push(result);
  }
  if (passing.length > 0) {
    passing.sort((a, b) => a.bytes - b.bytes);
    result = passing[Math.floor(passing.length / 2)];
  }
  out.tasks[id] = result;
  const outcome = result.error ? `error: ${result.error}` : result.passed ? 'passed' : 'FAILED';
  console.log(`${id}: ${result.calls} calls, ${result.bytes} bytes, ${outcome}`);
}
out.tasks = Object.fromEntries(Object.entries(out.tasks).sort(([a], [b]) => a.localeCompare(b)));
writeFileSync(path, JSON.stringify(out, null, 2) + '\n');
console.log(`wrote ${path.slice(root.length + 1)}`);
