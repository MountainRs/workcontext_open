#!/usr/bin/env python3
"""WorkContext Windows 冒烟 · 纠错落库验收（seed / verify-corrected / verify-restart）。

只操作 scripts/win-smoke.ps1 建立的冒烟沙箱库（--db 经 junction 指进沙箱）：

  seed             冷启动建库后，预置 A11「纠错前」状态（seed_a11_pre_correction.sql：
                   一条 active/me 的合成事项 + 证据 + created 事件）。只预置纠错前状态，
                   不伪造任何纠错结果；同时断言预置数据的范围隔离。快照 baseline。
  verify-corrected WebDriver 在 release 应用里真实点击「不是我的事」之后，核对应用自己
                   写出的纠错：belongs_to_me=false 与 lifecycle="ignored" 两把人工锁、
                   corrected(human) 事件、事项 lifecycle=ignored 且 attention=no_next、移出今日；
                   其余预置行原样。快照 baseline-post。
  verify-restart   再次冷启动并退出后：业务表与 baseline-post 逐行完全一致（人工锁与
                   纠错留痕跨重启持久化，未被静默恢复），schema 未变、integrity ok。

纠错断言口径按 fixtures/cases/A11_correction_lock/expected.json 的阶段 2：
点一次「不是我的事」即 lifecycle=ignored、attention=no_next、双锁、不再出现在今日。

只依赖 Python 3.8+ 标准库（sqlite3）。退出码：0 通过，1 失败，2 用法错误。
"""

import argparse
import json
import os
import re
import sqlite3
import sys
import time
from datetime import datetime, timezone

HERE = os.path.dirname(os.path.abspath(__file__))
SEED_SQL = os.path.join(HERE, "seed_a11_pre_correction.sql")

# 合成标识（与 seed_a11_pre_correction.sql 一致）
SRC = "wc-smoke-a11-src"
RECORD = SRC + "/A11-R1"
ITEM = "item-wc-smoke-a11"
RUN = "run-1a1fbb97dc0-4e4516616701-0000"
DB_FILENAME = "workcontext.sqlite3"  # crates/core/src/lib.rs DB_FILENAME

# 业务表全集（app_release 是应用自身的发布标记，不参与比对）。纯重启/一次纠错
# 之外的一切写入都会在这里暴露：范围隔离的兜底。
BUSINESS_TABLES = [
    ("source", "SELECT * FROM source ORDER BY id"),
    ("source_record", "SELECT * FROM source_record ORDER BY id"),
    ("agent_run", "SELECT * FROM agent_run ORDER BY id"),
    ("work_thread", "SELECT * FROM work_thread ORDER BY id"),
    ("work_item", "SELECT * FROM work_item ORDER BY id"),
    ("field_override", "SELECT * FROM field_override ORDER BY target_type, target_id, field"),
    ("work_event", "SELECT * FROM work_event ORDER BY target_type, target_id, at, id"),
    ("link_evidence", "SELECT * FROM link_evidence ORDER BY id"),
    ("sync_checkpoint", "SELECT * FROM sync_checkpoint ORDER BY source_id"),
    ("sync_run", "SELECT * FROM sync_run ORDER BY id"),
    ("egress_log", "SELECT * FROM egress_log ORDER BY id"),
    ("feedback_snapshot", "SELECT * FROM feedback_snapshot ORDER BY id"),
]
# 一次纠错只允许改动这三张表；其余业务表必须与 baseline 完全一致。
TOUCHED_BY_CORRECTION = {"work_item", "field_override", "work_event"}
# 纠错允许改动的 work_item 列（§9：只动用户表态的字段及其派生列）。
ITEM_MUTABLE = {"lifecycle", "attention", "awaiting_confirmation", "closed_at", "close_reason", "updated_at"}


def now_iso():
    return datetime.now(timezone.utc).isoformat()


def supported_version(migrations_dir):
    """与 store::supported_version 同口径：migrations/ 内最大编号。"""
    versions = [int(m.group(1)) for m in
                (re.match(r"^(\d{4})_.*\.sql$", name) for name in os.listdir(migrations_dir)) if m]
    if not versions:
        raise SystemExit(f"migrations 目录里没有 NNNN_*.sql：{migrations_dir}")
    return max(versions)


def same_path(a, b):
    return os.path.normcase(os.path.realpath(a)) == os.path.normcase(os.path.realpath(b))


def under(path, root):
    real_root = os.path.normcase(os.path.realpath(root))
    real_path = os.path.normcase(os.path.realpath(path))
    return real_path.startswith(real_root.rstrip(os.sep) + os.sep)


def connect(db_path):
    conn = sqlite3.connect(db_path, timeout=15)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA foreign_keys = ON")
    return conn


def rows(conn, sql):
    return [dict(r) for r in conn.execute(sql).fetchall()]


def snapshot(conn):
    return {table: rows(conn, sql) for table, sql in BUSINESS_TABLES}


def by_id(table_rows, key):
    return {r[key]: r for r in table_rows}


def apply_seed(conn, script_path):
    """单事务（BEGIN IMMEDIATE … COMMIT）执行 seed SQL；任一步失败即回滚。"""
    with open(script_path, "r", encoding="utf-8") as f:
        lines = f.read().splitlines()
    conn.execute("BEGIN IMMEDIATE")
    try:
        buffer = []
        for line in lines + [""]:
            buffer.append(line)
            candidate = "\n".join(buffer)
            if sqlite3.complete_statement(candidate):
                stmt = candidate.strip()
                if stmt:
                    conn.execute(stmt)
                buffer = []
        conn.execute("COMMIT")
    except Exception:
        conn.execute("ROLLBACK")
        raise


class Report:
    def __init__(self, phase):
        self.data = {"phase": phase, "ok": False, "startedAt": now_iso(), "checks": []}

    def check(self, name, ok, detail=""):
        self.data["checks"].append({"name": name, "ok": bool(ok), "detail": str(detail)})
        print(f"[{'PASS' if ok else 'FAIL'}] {name}" + (f" — {detail}" if not ok and detail else ""))
        return bool(ok)


def check_isolation(args, rep):
    """预置数据的范围隔离（一）：库与数据目录都必须落在沙箱里。"""
    sandbox = args.sandbox
    ok = under(args.db, sandbox)
    rep.check("库位于冒烟沙箱内（绝不指向日常用户库）", ok,
              f"db={os.path.realpath(args.db)} sandbox={os.path.realpath(sandbox)}")
    rep.check("库文件名与 DB_FILENAME 一致", os.path.basename(args.db) == DB_FILENAME, args.db)
    target = os.path.join(sandbox, "appdata")
    rep.check("应用数据目录（junction）解析进沙箱 appdata", same_path(args.app_data, target),
              f"appData={os.path.realpath(args.app_data)} expect={os.path.realpath(target)}")
    return ok and same_path(args.app_data, target)


def check_health(conn, args, rep):
    """库本身健康：schema 版本、WAL、完整性、外键。"""
    supported = supported_version(args.migrations)
    version = conn.execute("PRAGMA user_version").fetchone()[0]
    rep.data["user_version"] = version
    rep.data["supported_version"] = supported
    ok = rep.check("schema 版本 == migrations 最大编号", version == supported,
                   f"user_version={version} supported={supported}")
    mode = str(conn.execute("PRAGMA journal_mode").fetchone()[0]).lower()
    ok &= rep.check("WAL 生效（§4.1）", mode == "wal", f"journal_mode={mode}")
    integrity = conn.execute("PRAGMA integrity_check").fetchone()[0]
    ok &= rep.check("integrity_check == ok", integrity == "ok", integrity)
    fk = [tuple(r) for r in conn.execute("PRAGMA foreign_key_check").fetchall()]
    ok &= rep.check("foreign_key_check 为空", not fk, fk if fk else "")
    return ok


def load_baseline(path):
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)


def write_json(path, payload):
    with open(path, "w", encoding="utf-8") as f:
        json.dump(payload, f, ensure_ascii=False, indent=2)


# ---------------------------------------------------------------- seed
def expected_pre_state(conn, rep):
    """预置数据形态与范围隔离（二）：库里只有这些合成行，来源停用且未授权。"""
    snap = snapshot(conn)
    rep.data["counts"] = {t: len(v) for t, v in snap.items()}
    by = {t: by_id(v, "id") for t, v in snap.items() if v and "id" in v[0]}
    ok = True

    source = by.get("source", {}).get(SRC)
    ok &= rep.check("来源仅 1 条且为合成来源", len(snap["source"]) == 1 and source is not None,
                    json.dumps(snap["source"], ensure_ascii=False))
    if source:
        scope = json.loads(source["scope_json"])
        ok &= rep.check("合成来源已停用且未授权（同步引擎不会触碰）",
                        source["enabled"] == 0 and source["authorized_at"] is None,
                        f"enabled={source['enabled']} authorized_at={source['authorized_at']}")
        ok &= rep.check("合成来源 scope 只含虚构会话", scope == {"conversation_ids": ["dm-qian-manager"]},
                        source["scope_json"])

    record = by.get("source_record", {}).get(RECORD)
    ok &= rep.check("原始记录仅 1 条：A11-R1（dm / in / judged / dm_in）",
                    len(snap["source_record"]) == 1 and record is not None
                    and record["conversation_type"] == "dm" and record["direction"] == "in"
                    and record["judge_state"] == "judged" and record["prefilter_reason"] == "dm_in",
                    json.dumps(snap["source_record"], ensure_ascii=False))

    run = by.get("agent_run", {}).get(RUN)
    ok &= rep.check("判读记录仅 1 条：mock 运行时、status=ok",
                    len(snap["agent_run"]) == 1 and run is not None
                    and run["runtime"] == "mock" and run["status"] == "ok",
                    json.dumps(snap["agent_run"], ensure_ascii=False))

    item = by.get("work_item", {}).get(ITEM)
    ok &= rep.check("事项仅 1 条：纠错前 active / attention=me / 下一步非空",
                    len(snap["work_item"]) == 1 and item is not None
                    and item["lifecycle"] == "active" and item["attention"] == "me"
                    and bool((item["next_action"] or "").strip()),
                    json.dumps(snap["work_item"], ensure_ascii=False))
    ok &= rep.check("纠错前没有任何人工锁", snap["field_override"] == [],
                    json.dumps(snap["field_override"], ensure_ascii=False))
    events = [(e["event_type"], e["actor"], e["target_id"]) for e in snap["work_event"]]
    ok &= rep.check("纠错前事件仅 synced(system) + created(agent)",
                    events == [("synced", "system", RECORD), ("created", "agent", ITEM)], events)
    ok &= rep.check("纠错前没有任何 corrected 事件",
                    not [e for e in snap["work_event"] if e["event_type"] == "corrected"], "")
    ok &= rep.check("证据链仅 1 条：A11-R1 → 事项（origin/confirmed）",
                    len(snap["link_evidence"]) == 1
                    and snap["link_evidence"][0]["source_record_id"] == RECORD
                    and snap["link_evidence"][0]["target_id"] == ITEM,
                    json.dumps(snap["link_evidence"], ensure_ascii=False))
    ok &= rep.check("纠错前事项在今日可见范围（active）",
                    conn.execute("SELECT COUNT(*) FROM work_item WHERE lifecycle IN ('active','candidate')")
                    .fetchone()[0] == 1, "")
    silent = ["work_thread", "sync_checkpoint", "sync_run", "egress_log", "feedback_snapshot"]
    noisy = {t: len(snap[t]) for t in silent if snap[t]}
    ok &= rep.check("无脉络 / 同步游标 / 同步运行 / 出域 / 快照记录（不伪造同步与出域）",
                    not noisy, json.dumps(noisy, ensure_ascii=False))
    return ok


def phase_seed(args, rep):
    if not check_isolation(args, rep):
        return False
    conn = connect(args.db)
    try:
        if not check_health(conn, args, rep):
            return False
        before = snapshot(conn)
        dirty = {t: len(v) for t, v in before.items() if v}
        if not rep.check("冷启动建出的库业务表为空（沙箱干净）", not dirty,
                         json.dumps(dirty, ensure_ascii=False)):
            return False
        try:
            apply_seed(conn, SEED_SQL)
            rep.check("预置 A11 纠错前状态（单事务）", True, SEED_SQL)
        except Exception as e:  # noqa: BLE001 - 失败即整体回滚并如实报告
            return rep.check("预置 A11 纠错前状态（单事务）", False, f"{type(e).__name__}: {e}")
        ok = check_health(conn, args, rep)
        ok &= expected_pre_state(conn, rep)
        baseline = {
            "kind": "pre-correction",
            "seeded_at_ms": int(time.time() * 1000),
            "db": os.path.normcase(os.path.realpath(args.db)),
            "user_version": rep.data["user_version"],
            "tables": snapshot(conn),
        }
        write_json(args.baseline, baseline)
        ok &= rep.check("baseline（纠错前）已写出", True, args.baseline)
        return ok
    finally:
        conn.close()


# ---------------------------------------------------------------- verify-corrected
def verify_corrected_rows(conn, rep, base, note):
    """应用自己写出的纠错必须与 A11 阶段 2 一致；其余预置行原样。"""
    now = snapshot(conn)
    rep.data["counts"] = {t: len(v) for t, v in now.items()}
    ok = True

    untouched = {t: now[t] == base["tables"][t] for t, _ in BUSINESS_TABLES
                 if t not in TOUCHED_BY_CORRECTION}
    ok &= rep.check("未被纠错触及的业务表与 baseline 完全一致", all(untouched.values()),
                    json.dumps([t for t, same in untouched.items() if not same], ensure_ascii=False))
    ok &= rep.check("冒烟期间没有任何同步运行 / 出域记录",
                    now["sync_run"] == [] and now["egress_log"] == [] and now["sync_checkpoint"] == [],
                    json.dumps({t: now[t] for t in ("sync_run", "egress_log", "sync_checkpoint")},
                               ensure_ascii=False))

    # ---- work_item ----
    base_item = by_id(base["tables"]["work_item"], "id")[ITEM]
    items = by_id(now["work_item"], "id")
    item = items.get(ITEM)
    ok &= rep.check("事项仍然只有这一条", list(items) == [ITEM], list(items))
    if item:
        changed = sorted(k for k in item if item[k] != base_item.get(k))
        rep.data["item_after_correction"] = item
        ok &= rep.check("事项被置为 ignored（A11：立即移出本人关注）", item["lifecycle"] == "ignored",
                        f"lifecycle={item['lifecycle']}")
        ok &= rep.check("事项注意力被置为 no_next（不是我的事 ⇒ 不再轮到任何人）",
                        item["attention"] == "no_next", f"attention={item['attention']}")
        ok &= rep.check("忽略原因非空且为 not_mine", bool(item["close_reason"])
                        and item["close_reason"].startswith("not_mine"), repr(item["close_reason"]))
        ok &= rep.check("事项移出今日可见范围（无 active/candidate）",
                        conn.execute("SELECT COUNT(*) FROM work_item WHERE lifecycle IN ('active','candidate')")
                        .fetchone()[0] == 0, "")
        extra = sorted(set(changed) - ITEM_MUTABLE)
        ok &= rep.check("事项只改了表态字段及派生列（§9：不冻死/改动其余字段）", not extra, extra)

    # ---- field_override ----
    locks = {r["field"]: r for r in now["field_override"]}
    rep.data["locks_after_correction"] = now["field_override"]
    ok &= rep.check("人工锁恰好两把：belongs_to_me 与 lifecycle",
                    set(locks) == {"belongs_to_me", "lifecycle"}
                    and all(r["target_id"] == ITEM and r["target_type"] == "work_item"
                            for r in now["field_override"]),
                    json.dumps(sorted(locks), ensure_ascii=False))
    reason_text = "not_mine：" + note if note else "not_mine"
    stamps = []
    for field, value in (("belongs_to_me", "false"), ("lifecycle", '"ignored"')):
        lock = locks.get(field)
        ok &= rep.check(f"锁 {field}={value}、locked=1、set_by=human、note=原因文本",
                        lock is not None and lock["value_json"] == value and lock["locked"] == 1
                        and lock["set_by"] == "human" and lock["note"] == reason_text,
                        json.dumps(lock, ensure_ascii=False) if lock else "缺锁")
        if lock:
            stamps.append(lock["set_at"])

    # ---- work_event ----
    base_ids = {e["id"] for e in base["tables"]["work_event"]}
    new_events = [e for e in now["work_event"] if e["id"] not in base_ids]
    kept = [e for e in now["work_event"] if e["id"] in base_ids]
    ok &= rep.check("纠错前的事件原样保留", kept == base["tables"]["work_event"], "")
    ok &= rep.check("恰好新增 1 条事件（一次点击 = 一次纠错）", len(new_events) == 1,
                    json.dumps(new_events, ensure_ascii=False))
    event = new_events[0] if new_events else None
    if event:
        rep.data["event_after_correction"] = event
        after = json.loads(event["after_json"]) if event["after_json"] else {}
        ok &= rep.check("新事件为 corrected / human / note=not_mine，目标是该事项",
                        event["event_type"] == "corrected" and event["actor"] == "human"
                        and event["note"] == "not_mine" and event["target_type"] == "work_item"
                        and event["target_id"] == ITEM,
                        json.dumps({k: event[k] for k in ("event_type", "actor", "note", "target_id")}))
        ok &= rep.check("事件 after_json 记录锁定字段与备注",
                        set(after.get("fields", [])) == {"belongs_to_me", "lifecycle"}
                        and after.get("detail") == note, event["after_json"])
        ok &= rep.check("事件 id 形如 we-<at:x>-corrected-<事项>",
                        re.fullmatch(r"we-[0-9a-f]+-corrected-" + re.escape(ITEM) + r"(-\d+)?",
                                     event["id"]) is not None, event["id"])
        stamps.append(event["at"])

    # ---- 时序：这些行必须是 WebDriver 阶段由应用写出的 ----
    if stamps and item:
        ok &= rep.check("锁与事件同一事务同一时刻", len(set(stamps)) == 1, stamps)
        ok &= rep.check("写入时刻晚于预置（确由本次点击产生，而非预置数据）",
                        stamps[0] >= base["seeded_at_ms"],
                        f"written_at={stamps[0]} seeded_at={base['seeded_at_ms']}")
        ok &= rep.check("事项 updated_at / closed_at 与纠错同刻",
                        item["updated_at"] == stamps[0] and item["closed_at"] == stamps[0],
                        f"updated_at={item['updated_at']} closed_at={item['closed_at']}")
    return ok


def phase_verify_corrected(args, rep):
    if not check_isolation(args, rep):
        return False
    base = load_baseline(args.baseline)
    conn = connect(args.db)
    try:
        ok = check_health(conn, args, rep)
        ok &= rep.check("schema 版本与预置时一致", rep.data["user_version"] == base["user_version"],
                        f"now={rep.data['user_version']} baseline={base['user_version']}")
        ok &= verify_corrected_rows(conn, rep, base, args.note or "")
        write_json(args.baseline_out, {
            "kind": "post-correction",
            "db": base["db"],
            "user_version": rep.data["user_version"],
            "tables": snapshot(conn),
        })
        ok &= rep.check("baseline（纠错后）已写出", True, args.baseline_out)
        return ok
    finally:
        conn.close()


# ---------------------------------------------------------------- verify-restart
def phase_verify_restart(args, rep):
    if not check_isolation(args, rep):
        return False
    base = load_baseline(args.baseline)
    conn = connect(args.db)
    try:
        ok = rep.check("重启前后是同一个沙箱库文件", same_path(args.db, base["db"]),
                       f"{os.path.realpath(args.db)} vs {base['db']}")
        ok &= check_health(conn, args, rep)
        ok &= rep.check("schema 版本与纠错后一致（重启不重跑迁移）",
                        rep.data["user_version"] == base["user_version"],
                        f"now={rep.data['user_version']} baseline={base['user_version']}")
        now = snapshot(conn)
        rep.data["counts"] = {t: len(v) for t, v in now.items()}
        for table, _ in BUSINESS_TABLES:
            same = now[table] == base["tables"][table]
            if not same:
                base_rows = {json.dumps(r, ensure_ascii=False, sort_keys=True) for r in base["tables"][table]}
                now_rows = {json.dumps(r, ensure_ascii=False, sort_keys=True) for r in now[table]}
                detail = (f"缺失={sorted(base_rows - now_rows)}; 多出={sorted(now_rows - base_rows)}")
            else:
                detail = f"{len(now[table])} 行完全一致"
            ok &= rep.check(f"跨重启逐行一致：{table}", same, detail)
        locks = {r["field"]: (r["value_json"], r["locked"]) for r in now["field_override"]}
        ok &= rep.check("重启后人工锁仍生效：belongs_to_me=false、lifecycle=\"ignored\"",
                        locks == {"belongs_to_me": ("false", 1), "lifecycle": ('"ignored"', 1)},
                        json.dumps(locks, ensure_ascii=False))
        corrected = [e for e in now["work_event"]
                     if e["target_id"] == ITEM and e["event_type"] == "corrected"]
        ok &= rep.check("重启后纠错留痕仍在：corrected / human / not_mine",
                        len(corrected) == 1 and corrected[0]["actor"] == "human"
                        and corrected[0]["note"] == "not_mine",
                        json.dumps(corrected, ensure_ascii=False))
        ok &= rep.check("重启后未被静默恢复为本人待办（无 active/candidate）",
                        conn.execute("SELECT COUNT(*) FROM work_item WHERE lifecycle IN ('active','candidate')")
                        .fetchone()[0] == 0, "")
        return ok
    finally:
        conn.close()


PHASES = {
    "seed": phase_seed,
    "verify-corrected": phase_verify_corrected,
    "verify-restart": phase_verify_restart,
}


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="phase", required=True)
    for name in PHASES:
        p = sub.add_parser(name)
        p.add_argument("--db", required=True, help="沙箱库 workcontext.sqlite3（junction 指向）")
        p.add_argument("--app-data", required=True, help="应用的 app_data_dir（junction 路径）")
        p.add_argument("--sandbox", required=True, help="本次冒烟的沙箱根目录")
        p.add_argument("--migrations", required=True, help="仓库 migrations/ 目录")
        p.add_argument("--report", required=True, help="JSON 报告输出")
        p.add_argument("--baseline", required=True, help="seed: 写出；verify-*: 读取")
        p.add_argument("--baseline-out", help="verify-corrected: 纠错后快照输出")
        p.add_argument("--note", default="", help="verify-corrected: WebDriver 填写的纠错备注")
    args = ap.parse_args()

    rep = Report(args.phase)
    rep.data.update({
        "db": os.path.abspath(args.db),
        "sandbox": os.path.abspath(args.sandbox),
        "appData": os.path.abspath(args.app_data),
    })
    try:
        if not os.path.isfile(args.db):
            raise SystemExit(f"库不存在（{args.db}）：{args.phase} 必须在冷启动建库之后运行")
        PHASES[args.phase](args, rep)
    except SystemExit as e:
        rep.check("执行中断", False, e)
    except Exception as e:  # noqa: BLE001 - 如实落报告
        import traceback
        rep.check("执行中断", False, f"{type(e).__name__}: {e}\n{traceback.format_exc()}")

    rep.data["ok"] = bool(rep.data["checks"]) and all(c["ok"] for c in rep.data["checks"])
    rep.data["finishedAt"] = now_iso()
    write_json(args.report, rep.data)
    print(f"{args.phase}: {'OK' if rep.data['ok'] else 'FAILED'}")
    sys.exit(0 if rep.data["ok"] else 1)


if __name__ == "__main__":
    main()
