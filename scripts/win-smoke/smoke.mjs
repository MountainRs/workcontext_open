#!/usr/bin/env node
/**
 * WorkContext Windows 冒烟 · WebDriver 阶段（cold / correct / restart），由 scripts/win-smoke.ps1 调度。
 *
 * 驱动方式对齐官方 tauri-driver + Selenium 方案（https://tauri.app/develop/tests/webdriver/）：
 * tauri-driver（--native-driver 指向沙箱里与 WebView2 运行时同版本的 msedgedriver）以
 * capabilities { browserName: 'wry', 'tauri:options': { application: <release exe> } } 拉起
 * 真实的 release WorkContext；以下所有断言都发生在真实应用窗口里，经真实 IPC 读本地库。
 *
 *   cold     冷启动：应用在沙箱建库；真实点击侧边栏切三页（今日 /today → 全部事项 /items →
 *            迭代池 /pool），逐页断言 URL / H1 / 数据经 IPC 加载 / 无错误提示并截图；
 *            空库下今日「轮到我」0 条、全部事项 0 行。
 *   correct  预置纠错前数据后：今日「轮到我」恰好 1 条合成事项、列表恰好 1 行（UI 层的范围
 *            隔离）；进入详情页，填写备注并点击「不是我的事」，等待「已纠正」反馈；断言
 *            人工锁字段、纠错历史、生命周期=ignored；回到今日确认已移出「轮到我」（A11）。
 *            落库内容随后由 seed_verify.py verify-corrected 逐行核对。
 *   restart  再次冷启动：三页可切，合成事项已不在今日；从列表进入详情，人工锁与纠错留痕
 *            仍在（跨重启持久化）。落库内容由 verify-restart 逐行核对。
 *
 * 隔离：本脚本不接触任何用户数据目录；数据目录重定向（junction）与
 * WEBVIEW2_USER_DATA_FOLDER 由 win-smoke.ps1 设置好后再启动。全程不点同步/授权/登录，
 * 不访问企业内容。
 *
 * 依赖：Node >= 22（锁定的 selenium-webdriver 4.51 的要求）；selenium-webdriver 见同目录
 * package.json，由 win-smoke.ps1 的 npm ci 安装。
 * 退出码：0 通过，1 失败，2 用法错误，3 看门狗超时（单阶段 6 分钟上限）。
 */

import { spawn } from "node:child_process";
import { createConnection } from "node:net";
import fs from "node:fs";
import path from "node:path";
import process from "node:process";

import { Builder, By, Capabilities, until } from "selenium-webdriver";

const KNOWN_ARGS = new Set([
  "phase", "exe", "tauri-driver", "native-driver", "db", "out", "port", "native-port", "note",
]);
const WATCHDOG_MS = 6 * 60 * 1000;
const DOCUMENT_TITLE = "个人AI工作台";
const ITEM_LINK = '/items/item-wc-smoke-a11';
const XPATH_LOCKS =
  "//section[@aria-labelledby='detail-next']//dt[normalize-space(text())='人工锁字段']/following-sibling::dd[1]";
const XPATH_LIFECYCLE =
  "//div[contains(@class,'detail-meta')]/span[starts-with(normalize-space(.),'生命周期：')]";
const XPATH_NOT_MINE =
  "//section[@aria-labelledby='detail-correct']//button[normalize-space()='不是我的事']";

const args = parseArgs(process.argv.slice(2));
const report = {
  phase: args.phase,
  ok: false,
  startedAt: new Date().toISOString(),
  exe: args.exe,
  db: args.db,
  checks: [],
  pages: [],
  artifacts: [],
};

let driver = null;
let tauriDriverProc = null;

let watchdog = null;

async function entry() {
  watchdog = setTimeout(() => {
    check("看门狗（单阶段 6 分钟上限）", false, "超时强杀");
    finish(3);
  }, WATCHDOG_MS);
  try {
    await main();
  } catch (error) {
    check("执行中断", false, error?.stack || String(error));
  } finally {
    await teardown();
  }
  finish(report.checks.length > 0 && report.checks.every((c) => c.ok) ? 0 : 1);
}


// ------------------------------------------------------------------ 流程
async function main() {
  requireArgs("phase", "exe", "tauriDriver", "nativeDriver", "db", "out");
  const t0 = Date.now();

  tauriDriverProc = spawn(
    args.tauriDriver,
    [
      "--port",
      String(args.port),
      "--native-port",
      String(args.nativePort),
      "--native-driver",
      args.nativeDriver,
    ],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  const pipe = (stream) =>
    stream.on("data", (chunk) => log("tauri-driver", chunk.toString().trim()));
  pipe(tauriDriverProc.stdout);
  pipe(tauriDriverProc.stderr);

  if (!(await waitTcp(Number(args.port), 30000))) {
    return check("tauri-driver 监听就绪", false, `127.0.0.1:${args.port} 30s 内未监听`);
  }

  let sessionError = "";
  for (let attempt = 1; attempt <= 3 && !driver; attempt++) {
    try {
      const capabilities = new Capabilities();
      capabilities.set("tauri:options", { application: path.resolve(args.exe) });
      capabilities.setBrowserName("wry");
      driver = await new Builder()
        .usingServer(`http://127.0.0.1:${args.port}`)
        .withCapabilities(capabilities)
        .build();
    } catch (error) {
      sessionError = String(error?.message || error);
      await sleep(4000);
    }
  }
  if (!check("WebDriver 会话建立（wry / tauri:options.application = release exe）", !!driver, sessionError)) {
    return;
  }
  report.timings = { sessionMs: Date.now() - t0 };

  const tNav = Date.now();
  await driver.wait(until.elementsLocated(By.css('nav[aria-label="主导航"]')), 45000);
  report.timings.firstNavMs = Date.now() - tNav;

  // WebDriver getTitle() 读的是 document.title（ui/index.html 的 <title>），
  // 不是 Tauri 配置里的原生窗口标题。
  const title = await driver.getTitle();
  check("页面标题为「个人AI工作台」（release 前端已加载）", title === DOCUMENT_TITLE, `getTitle()=${title}`);
  const initialUrl = await driver.getCurrentUrl();
  check("冷启动首屏自动进入 /today", /\/today\/?$/.test(initialUrl), initialUrl);

  if (args.phase === "cold") {
    const tDb = Date.now();
    const dbStat = await waitForDb(60000);
    if (!check("应用在沙箱数据目录建库（workcontext.sqlite3）", !!dbStat,
               dbStat ? `${dbStat.size} bytes @ ${Date.now() - tDb}ms` : "60s 内未出现")) {
      return;
    }
  } else if (!check("建会话前沙箱库已存在且非空（沿用同一库）", !!statDb(), args.db)) {
    return;
  }

  const facts = {};
  for (const page of PAGES) {
    facts[page.key] = await gotoPage(page);
  }
  if (Object.values(facts).some((f) => !f || !f.ok)) return;

  if (args.phase === "cold") {
    check("空库冷启动：今日「轮到我」0 条（沙箱里没有任何日常数据）", facts.today.mineCount === 0,
          `mineCount=${facts.today.mineCount}`);
    check("空库冷启动：全部事项 0 行", facts.items.rows === 0, `rows=${facts.items.rows}`);
  } else if (args.phase === "correct") {
    await phaseCorrect(facts);
  } else {
    await phaseRestart(facts);
  }
}

async function phaseCorrect(facts) {
  check("纠错前：今日「轮到我」恰好 1 条合成事项（A11 阶段 1）",
        facts.today.mineCount === 1 && facts.today.itemCard,
        `mineCount=${facts.today.mineCount} itemCard=${facts.today.itemCard}`);
  check("范围隔离（UI）：全部事项恰好 1 行，且就是这条合成事项",
        facts.items.rows === 1 && facts.items.itemLink, JSON.stringify(facts.items));

  await openDetail("today");
  if (!check("纠错前：详情页人工锁字段为「无」", (await text(XPATH_LOCKS, By.xpath)) === "无",
             await text(XPATH_LOCKS, By.xpath))) return;
  check("纠错前：详情页生命周期为 active", (await text(XPATH_LIFECYCLE, By.xpath)) === "生命周期：active",
        await text(XPATH_LIFECYCLE, By.xpath));
  const history = await text('section[aria-labelledby="detail-corrections"]');
  if (!check("纠错前：还没有人工纠错", history.includes("还没有人工纠错"), history)) return;
  await screenshot("correct-1-detail-before");

  // 填写备注并点击「不是我的事」（一次点击即写入，UI 无二次确认）
  const note = args.note || "";
  const input = await driver.wait(
    until.elementLocated(By.css('section[aria-labelledby="detail-correct"] > label.field input.field-input')),
    15000,
  );
  await input.clear();
  if (note) await input.sendKeys(note);
  const typed = await input.getProperty("value");
  if (!check("填写纠错备注", typed === note, `实际输入=${JSON.stringify(typed)}`)) return;
  await screenshot("correct-2-note-typed");

  await driver.findElement(By.xpath(XPATH_NOT_MINE)).click();
  const outcome = await waitForOutcome(20000);
  report.correction = { note, feedback: outcome.text };
  if (!check("点击「不是我的事」后应用反馈已纠正", outcome.ok, outcome.text)) return;

  const locks = await waitText(XPATH_LOCKS, By.xpath, /belongs_to_me/, 15000);
  const lifecycle = await waitText(XPATH_LIFECYCLE, By.xpath, /ignored/, 15000);
  const correctedHistory = await waitText('section[aria-labelledby="detail-corrections"]', By.css, /not_mine/, 15000);
  const timeline = await text("ol.timeline");
  check("纠错后：人工锁字段含 belongs_to_me 与 lifecycle",
        locks.includes("belongs_to_me") && locks.includes("lifecycle"), locks);
  check("纠错后：详情页生命周期为 ignored", lifecycle === "生命周期：ignored", lifecycle);
  check("纠错后：纠错历史记录 not_mine、锁定字段与备注",
        correctedHistory.includes("not_mine") && correctedHistory.includes("belongs_to_me")
          && correctedHistory.includes("lifecycle") && (!note || correctedHistory.includes(note)),
        correctedHistory);
  check("纠错后：事件时间线出现「人工纠错」", timeline.includes("人工纠错"), timeline);
  await screenshot("correct-3a-detail-after");
  await scrollTo('section[aria-labelledby="detail-corrections"]');
  await screenshot("correct-3b-detail-corrections");

  // A11：点一次即移出今日「轮到我」
  const after = await gotoPage(PAGES[0], "correct-4-today-after");
  check("A11：点一次「不是我的事」即移出今日「轮到我」", after.mineCount === 0 && !after.itemCard,
        `mineCount=${after.mineCount} itemCard=${after.itemCard}`);

  await gotoPage(PAGES[1], "correct-5-items-after");
  await clickFilter("轮到我");
  const goneFromMine = await waitGone(itemLinkCss("table"), 15000);
  check("A11：列表「轮到我」筛选不再包含该事项", goneFromMine, await itemsRowsText());
  await clickFilter("已收尾");
  const inClosed = await waitPresent(itemLinkCss("table"), 15000);
  check("A11：列表「已收尾」筛选包含该事项（ignored）", inClosed, await itemsRowsText());
  await screenshot("correct-6-items-closed-filter");
}

async function phaseRestart(facts) {
  check("重启后：今日「轮到我」为 0（纠错状态跨重启保持，未被恢复）",
        facts.today.mineCount === 0 && !facts.today.itemCard,
        `mineCount=${facts.today.mineCount} itemCard=${facts.today.itemCard}`);
  check("重启后：全部事项仍有这 1 条合成事项", facts.items.rows === 1 && facts.items.itemLink,
        JSON.stringify(facts.items));

  await openDetail("items");
  const locks = await waitText(XPATH_LOCKS, By.xpath, /lifecycle/, 15000);
  const lifecycle = await text(XPATH_LIFECYCLE, By.xpath);
  const history = await text('section[aria-labelledby="detail-corrections"]');
  const note = args.note || "";
  check("重启后：人工锁仍为 belongs_to_me 与 lifecycle",
        locks.includes("belongs_to_me") && locks.includes("lifecycle"), locks);
  check("重启后：生命周期仍为 ignored", lifecycle === "生命周期：ignored", lifecycle);
  check("重启后：纠错历史仍在（not_mine + 备注）",
        history.includes("not_mine") && (!note || history.includes(note)), history);
  await screenshot("restart-1a-detail");
  await scrollTo('section[aria-labelledby="detail-corrections"]');
  await screenshot("restart-1b-detail-corrections");
}

// ------------------------------------------------------------------ 页面与元素
const PAGES = [
  {
    key: "today",
    route: "/today",
    h1: "今日",
    ready: 'section[aria-labelledby="lane-mine"]',
    describe: async () => ({
      mineCount: Number((await attr('section[aria-labelledby="lane-mine"] .lane-count', "aria-label")).match(/(\d+)/)?.[1]),
      itemCard: await exists(itemLinkCss('section[aria-labelledby="lane-mine"]')),
    }),
  },
  {
    key: "items",
    route: "/items",
    h1: "全部事项",
    ready: 'table[aria-label="事项列表"] td.cell-title, table[aria-label="事项列表"] td.items-empty-cell',
    describe: async () => ({
      rows: (await driver.findElements(By.css('table[aria-label="事项列表"] tbody td.cell-title'))).length,
      itemLink: await exists(itemLinkCss('table[aria-label="事项列表"]')),
      footnote: await text("p.table-footnote"),
    }),
  },
  {
    key: "pool",
    route: "/pool",
    h1: "迭代池",
    ready: 'section[aria-labelledby="pool-create-title"]',
    describe: async () => ({
      createPanel: await exists('section[aria-labelledby="pool-create-title"]'),
      exportGroup: await exists('[aria-label="导出快照"]'),
    }),
  },
];

/** 链接按 href 后缀匹配：应用用 HashRouter（href="#/today"），后缀写法对 BrowserRouter 同样成立。 */
function itemLinkCss(scope) {
  return `${scope} a[href$="${ITEM_LINK}"]`;
}

/** 真实点击侧边栏切页；断言 URL / H1 / 数据经 IPC 加载完成 / 无错误提示。 */
async function gotoPage(page, shotName) {
  const t = Date.now();
  const link = await driver.wait(
    until.elementLocated(By.css(`nav[aria-label="主导航"] a[href$="${page.route}"]`)), 15000);
  await link.click();
  await driver.wait(until.urlMatches(new RegExp(`${page.route}/?$`)), 15000);
  await waitText("main h1.page-title", By.css, new RegExp(`^${page.h1}$`), 15000);
  await driver.wait(until.elementLocated(By.css(page.ready)), 30000);
  await waitGone("main .loading-line", 15000);
  const errors = await pageErrors();
  const ok = check(`切页 ${page.route}：H1=${page.h1}，数据经 IPC 加载，无错误提示`,
                   errors.length === 0, errors.join(" | "));
  const facts = await page.describe();
  const shot = await screenshot(shotName || `${args.phase}${page.route.replace(/\//g, "-")}`);
  report.pages.push({ route: page.route, url: await driver.getCurrentUrl(), ms: Date.now() - t, facts, screenshot: shot });
  return { ...facts, ok };
}

/** 先切回来源页，再从该页的卡片/行链接进入合成事项详情。 */
async function openDetail(from) {
  const page = PAGES.find((p) => p.key === from);
  await gotoPage(page, `${args.phase}-before-detail`);
  const link = await driver.wait(until.elementLocated(By.css(itemLinkCss(
    from === "today" ? 'section[aria-labelledby="lane-mine"]' : 'table[aria-label="事项列表"]'))), 30000);
  await link.click();
  await driver.wait(until.urlMatches(new RegExp(`${ITEM_LINK}/?$`)), 15000);
  await waitText("h1.detail-title", By.css, /^二期图纸复核推广跟进$/, 30000);
  await driver.wait(until.elementLocated(By.css('section[aria-labelledby="detail-corrections"]')), 30000);
  const errors = await pageErrors();
  check(`从${page.h1}进入合成事项详情页（item_get 经 IPC 读取）`, errors.length === 0, errors.join(" | "));
}

async function clickFilter(label) {
  await driver
    .findElement(By.xpath(`//div[@aria-label="按状态筛选"]//button[normalize-space()='${label}']`))
    .click();
  await waitGone("main .loading-line", 15000);
  const errors = await pageErrors();
  check(`列表筛选「${label}」`, errors.length === 0, errors.join(" | "));
}

async function itemsRowsText() {
  const cells = await driver.findElements(By.css('table[aria-label="事项列表"] tbody td.cell-title'));
  return cells.length ? (await Promise.all(cells.map((c) => c.getText()))).join(" | ") : "（空）";
}

/** 纠错提交后的结果：成功反馈或错误提示，二者先到先得。 */
async function waitForOutcome(timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const ok = await driver.findElements(By.css('main .inline-feedback.ok[role="status"]'));
    if (ok.length && /已纠正：不是我的事/.test(await ok[0].getText())) {
      return { ok: true, text: normalize(await ok[0].getText()) };
    }
    const errors = await pageErrors();
    if (errors.length) return { ok: false, text: errors.join(" | ") };
    await sleep(250);
  }
  return { ok: false, text: `${timeoutMs}ms 内未出现「已纠正」反馈` };
}

async function pageErrors() {
  const els = await driver.findElements(By.css("main .notice.error, main .inline-feedback.error"));
  return Promise.all(els.map((el) => normalize(el.getText())));
}

async function attr(css, name) {
  return String((await (await driver.findElement(By.css(css))).getAttribute(name)) ?? "");
}

async function exists(css) {
  return (await driver.findElements(By.css(css))).length > 0;
}

async function text(selector, by = By.css) {
  return normalize(await driver.findElement(by(selector)).getText());
}

function normalize(value) {
  return String(value ?? "").replace(/\s+/g, " ").trim();
}

/** 轮询直到元素文本匹配；超时返回当时文本（由调用方断言）。 */
async function waitText(selector, by, pattern, timeoutMs) {
  let last = "";
  try {
    await driver.wait(async () => {
      const els = await driver.findElements(by(selector));
      if (!els.length) return false;
      last = normalize(await els[0].getText());
      return pattern.test(last);
    }, timeoutMs);
  } catch {
    // 超时：返回最后一次读到的文本供断言与报告
  }
  return last;
}

async function waitGone(css, timeoutMs) {
  try {
    await driver.wait(async () => !(await exists(css)), timeoutMs);
    return true;
  } catch {
    return false;
  }
}

async function waitPresent(css, timeoutMs) {
  try {
    await driver.wait(() => exists(css), timeoutMs);
    return true;
  } catch {
    return false;
  }
}

async function screenshot(name) {
  const file = path.join(args.out, `shot-${name}.png`);
  fs.writeFileSync(file, Buffer.from(await driver.takeScreenshot(), "base64"));
  report.artifacts.push(file);
  return file;
}

/** 截图只拍可视区域：先把目标区块滚进视口，让留痕进入 CI 产物。 */
async function scrollTo(css) {
  await driver.executeScript(
    "arguments[0].scrollIntoView({ block: 'center' })",
    await driver.findElement(By.css(css)),
  );
  await sleep(300);
}

// ------------------------------------------------------------------ 基础设施
function check(name, ok, detail = "") {
  report.checks.push({ name, ok: !!ok, detail: String(detail) });
  log(ok ? "PASS" : "FAIL", `${name}${!ok && detail ? ` — ${detail}` : ""}`);
  return !!ok;
}

function statDb() {
  try {
    const s = fs.statSync(args.db);
    return s.size > 0 ? { size: s.size } : null;
  } catch {
    return null;
  }
}

async function waitForDb(timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const s = statDb();
    if (s) return s;
    await sleep(200);
  }
  return null;
}

async function teardown() {
  report._closing = true;
  try {
    if (driver) await driver.quit();
  } catch {
    // 会话可能已不存在
  }
  driver = null;
  if (tauriDriverProc && tauriDriverProc.exitCode === null) {
    try {
      await Promise.race([
        new Promise((resolve) => tauriDriverProc.once("exit", resolve)),
        sleep(10000),
      ]);
    } finally {
      if (tauriDriverProc.exitCode === null) tauriDriverProc.kill("SIGKILL");
    }
  }
}

function finish(code) {
  clearTimeout(watchdog);
  report.ok = code === 0 && report.checks.length > 0 && report.checks.every((c) => c.ok);
  report.finishedAt = new Date().toISOString();
  if (args.out) {
    try {
      fs.mkdirSync(args.out, { recursive: true });
      fs.writeFileSync(path.join(args.out, `report-${args.phase}.json`), JSON.stringify(report, null, 2));
    } catch (error) {
      log("WARN", `报告写出失败: ${error}`);
    }
  }
  log("DONE", `${args.phase}: ${report.ok ? "OK" : "FAILED"}`);
  process.exit(report.ok ? 0 : code);
}

function waitTcp(port, timeoutMs) {
  return new Promise((resolve) => {
    const deadline = Date.now() + timeoutMs;
    const attempt = () => {
      const socket = createConnection({ port, host: "127.0.0.1" }, () => {
        socket.destroy();
        resolve(true);
      });
      socket.on("error", () => {
        if (Date.now() > deadline) resolve(false);
        else setTimeout(attempt, 300);
      });
    };
    attempt();
  });
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function log(tag, message) {
  if (message) console.log(`[${args.phase}] [${tag}] ${message}`);
}

/** `--kebab-case value` 成对出现；键归一为 camelCase，未知或缺值的参数直接按用法错误退出。 */
function parseArgs(argv) {
  const parsed = { port: 4444, nativePort: 4445, note: "" };
  for (let i = 0; i < argv.length; i += 2) {
    const raw = String(argv[i]);
    const name = raw.replace(/^--/, "");
    if (!raw.startsWith("--") || !KNOWN_ARGS.has(name) || i + 1 >= argv.length) {
      console.error(`无法识别或不完整的参数: ${raw}（参数须 --key value 成对出现）`);
      process.exit(2);
    }
    parsed[name.replace(/-([a-z])/g, (_, c) => c.toUpperCase())] = argv[i + 1];
  }
  return parsed;
}

function requireArgs(...names) {
  const missing = names.filter((n) => !args[n]);
  if (missing.length) {
    console.error(`缺少参数: ${missing.map((m) => `--${m.replace(/[A-Z]/g, (c) => `-${c.toLowerCase()}`)}`).join(" ")}`);
    process.exit(2);
  }
}

// 所有声明就位后再启动（避免临时死区）
entry();
