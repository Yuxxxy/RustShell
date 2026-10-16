#!/usr/bin/env python3
"""对 scripts/check-release-workflow.py 的负例测试。

思路：把真 workflow 复制到 .tmp/，逐个注入一种「静默错」，断言预检必须红。
只断言「必须红」不够 —— 还要断言「红的原因对」，否则脚本可能因为别的原因误报。

用法（Windows 本机需指定 Git Bash）：
  set CHECK_BASH=C:\\...\\bin\\bash.exe
  python scripts/selftest-check-release-workflow.py
"""
import os
import shutil
import subprocess
import sys
from pathlib import Path

REAL_REPO = Path(__file__).resolve().parent.parent
REAL_WF = REAL_REPO / ".github" / "workflows" / "release.yml"
TMP = REAL_REPO / ".tmp" / "checker_selftest"
BASH = os.environ.get("CHECK_BASH") or "bash"

# 造一个「假仓库」：脚本按 __file__ 定位仓库根，所以把它拷进假仓库的 scripts/ 下，
# Cargo.toml 也放假的那一份 —— 于是版本号类用例可以在不影响真仓库的前提下构造。
FAKE = TMP / "fake_repo"
(FAKE / "scripts").mkdir(parents=True, exist_ok=True)
shutil.copy(REAL_REPO / "scripts" / "check-release-workflow.py",
            FAKE / "scripts" / "check-release-workflow.py")

LF = REAL_WF.read_bytes().decode("utf-8").replace("\r\n", "\n")

CASES = []


def mutate(text, old, new):
    """注入变异，并断言真的打上了 —— 变异没生效会伪装成「护栏没抓住」。"""
    if old not in text:
        raise SystemExit(f"变异目标不存在，用例本身写错了：{old[:60]!r}")
    return text.replace(old, new, 1)


def case(name, expect_rc, expect_in_err, workflow_bytes=None, cargo=None):
    CASES.append((name, expect_rc, expect_in_err, workflow_bytes, cargo))


case("基线：真文件原样（本机工作树是 CRLF）", 0, None)
case("行内孤立 CR 混进 run 块", 1, "孤立 CR",
     workflow_bytes=mutate(LF,
         '          set -euo pipefail\n          # 两个变量都先点名',
         '          set -euo pipefail\rX\n          # 两个变量都先点名').encode())
case("零宽空格 U+200B 混进脚本", 1, "U+200B",
     workflow_bytes=mutate(LF, 'STAGE="dist/$PKG"',
                           'STAGE="dist/\u200b$PKG"').encode())
case("run 块 shell 语法错（if 不闭合）", 1, "bash -n",
     workflow_bytes=mutate(LF,
         '          set -euo pipefail\n          # 两个变量都先点名',
         '          if [ -n "$NOPE" ]; then\n          # 两个变量都先点名').encode())
case("引用了没声明的 step 输出", 1, "outputs.pkgg",
     workflow_bytes=mutate(LF, "steps.ver.outputs.pkg",
                           "steps.ver.outputs.pkgg").encode())
case("版本号含空格（模拟解析静默失败）", 1, "文件名非法字符",
     cargo='version = "1.0.0 beta"\n')
case("Cargo.toml 没有 ^version 行", 1, "取不到版本号",
     cargo='[package]\nname = "rustshell"\n')

fail_rc = 0
for name, expect_rc, expect_msg, wf_bytes, cargo in CASES:
    (FAKE / "Cargo.toml").write_text(cargo or 'version = "1.0.0"\n', encoding="utf-8")
    if wf_bytes is None:
        (TMP / "wf.yml").write_bytes(REAL_WF.read_bytes())
        wf_path = TMP / "wf.yml"    # 内容与真文件逐字节相同（含本机 CRLF 工作树形态）
    else:
        (TMP / "wf.yml").write_bytes(wf_bytes)
        wf_path = TMP / "wf.yml"

    env = dict(os.environ, CHECK_BASH=BASH, CHECK_WORKFLOW=str(wf_path))
    r = subprocess.run(
        [sys.executable, str(FAKE / "scripts" / "check-release-workflow.py")],
        capture_output=True, text=True, env=env, cwd=str(FAKE))
    out = r.stdout + r.stderr
    ok_rc = (r.returncode == expect_rc)
    ok_msg = (expect_msg is None) or (expect_msg in out)
    mark = "PASS" if (ok_rc and ok_msg) else "FAIL"
    if mark == "FAIL":
        fail_rc = 1
    print(f"[{mark}] {name}: exit={r.returncode}（期望 {expect_rc}）"
          f"，关键字 {expect_msg!r} {'命中' if ok_msg else '未命中'}")
    if mark == "FAIL":
        print("      --- 输出 ---")
        for line in out.strip().split("\n"):
            print(f"      {line}")

shutil.rmtree(TMP, ignore_errors=True)
print("\n总体：", "全部通过" if fail_rc == 0 else "有用例不符合预期")
sys.exit(fail_rc)
