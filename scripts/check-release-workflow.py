#!/usr/bin/env python3
"""release.yml 预检：在起 1~2 小时的编译**之前**，先把几类「静默错」挡在门口。

为什么需要它（2026-10-16 那两次发布失败，每一类都能在这里拦住）：

  1. 不可见字符 / 换行
     workflow 里混进一个孤立 CR 或零宽空格，Linux / macOS 上的 bash 会把整行
     当成选项名或命令名，报出来的错跟真实原因毫无关系（"invalid option name"、
     `$'\\r': command not found`），排查成本极高。
     注：git 只会把 `\\r\\n` 规范化为 `\\n`（仓库里另有 .gitattributes 兜底），
     所以**真正能活到 blob 里的只有行内的孤立 CR** —— 本脚本只报那一种。

  2. run 块的 shell 语法
     每个 `run: |` / `run: >` 的正文都过一遍 `bash -n`（先把 `${{ ... }}` 换成哑值，
     与 GitHub「先插值再执行」的口径一致）。

  3. 版本号
     从 Cargo.toml 解析 + 字符集校验。那次 macOS 失败就是 BSD sed 不认 `\\s`、
     正则匹配不到却**静默输出整行**，产物名里于是带上了双引号。

  4. `steps.<id>.outputs.<key>` 引用
     GitHub 对这种引用**求值成空串且不报错** —— 于是表现为「变量莫名变空」，
     最后以「文件名含非法字符」之类完全无关的报错收场。这里要求每个被引用的
     输出都能在同 id 的 step 里找到写入口。

  5. `$VAR` 后紧跟非 ASCII 字符
     macOS runner 的 `/bin/bash` 是 **3.2**，它会把紧跟变量名的全角标点算进变量名：
     `"$VERSION（CFBundle...）"` 被解析成变量 `VERSION（`，在 `set -u` 下直接报
     `VERSION?: unbound variable`；而 Linux 的 bash 5 + glibc 不会 —— 同一个脚本
     因此只有 macOS job 红（2026-10-18 首次跑 macOS 打包就栽在这行）。
     正确写法是 `${VERSION}（...`。本项同时扫 workflow 正文与 `scripts/*.sh`。

用法：
    python3 scripts/check-release-workflow.py

环境变量：
    CHECK_WORKFLOW   覆盖 workflow 路径（默认 <repo>/.github/workflows/release.yml）
    CHECK_BASH       指定 bash 可执行文件（Windows 本机需指向 Git Bash）

退出码：0 = 通过；1 = 有阻断项（消息前缀 `[!!]`）
"""
import os
import re
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
WORKFLOW = Path(
    os.environ.get("CHECK_WORKFLOW")
    or (REPO / ".github" / "workflows" / "release.yml")
)
CARGO_TOML = REPO / "Cargo.toml"
BASH = os.environ.get("CHECK_BASH") or "bash"

PROBLEMS = []


def ok(msg):
    print("  [ok]   " + msg)


def bad(msg):
    PROBLEMS.append(msg)
    print("  [!!]   " + msg)


def show(path):
    """相对仓库根的展示名；不在仓库内就退回绝对路径（不抛异常）。"""
    try:
        return path.relative_to(REPO).as_posix()
    except ValueError:
        return str(path)


# --------------------------------------------------------------------------- #
# 1. 不可见字符与换行
# --------------------------------------------------------------------------- #

ZERO_WIDTH = {
    "\u200b": "零宽空格 U+200B",
    "\u200c": "零宽非连接符 U+200C",
    "\u200d": "零宽连接符 U+200D",
    "\ufeff": "BOM / 零宽不换行空格 U+FEFF",
    "\u00a0": "不换行空格 U+00A0",
    "\u2028": "行分隔符 U+2028",
    "\u2029": "段分隔符 U+2029",
}


def check_invisible(raw):
    """报出不可见字符，返回「行尾已归一化为 LF」的文本供后续检查使用。

    必须以二进制读入再自己解码：`Path.read_text()` 默认做 universal newlines
    转换，会把 `\\r` 直接吃掉 —— 那样这个检查就永远看不到自己要查的东西。
    """
    print("[1/5] 不可见字符与换行")
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as exc:
        bad(show(WORKFLOW) + " 不是合法 UTF-8：" + str(exc))
        return ""

    if raw.startswith(b"\xef\xbb\xbf"):
        bad(show(WORKFLOW) + " 含 UTF-8 BOM —— YAML 首行会解析失败")

    # git 会规范化 \r\n；只有「后面不跟 \n 的 \r」才会原样进仓库
    normalized = text.replace("\r\n", "\n")
    hits = 0
    for lineno, line in enumerate(normalized.split("\n"), 1):
        for col, ch in enumerate(line, 1):
            if ch == "\r":
                bad(
                    "%s:%d:%d 含孤立 CR（行内回车）：git 不会替你规范化它，会一路进 CI，"
                    "bash 报出来的错与真实原因无关。上下文 %r"
                    % (show(WORKFLOW), lineno, col, line.strip()[:60])
                )
                hits += 1
            elif ch in ZERO_WIDTH:
                bad(
                    "%s:%d:%d 含 %s。上下文 %r"
                    % (show(WORKFLOW), lineno, col, ZERO_WIDTH[ch], line.strip()[:60])
                )
                hits += 1
    if not hits:
        ok("没有孤立 CR / 零宽字符 / BOM / 不换行空格")
    return normalized


# --------------------------------------------------------------------------- #
# 2. run 块的 shell 语法
# --------------------------------------------------------------------------- #

RE_RUN_HEAD = re.compile(r"^(\s*)(?:-\s+)?run:\s*[|>][-+]?\s*$")


def extract_run_blocks(text):
    """提取所有 `run: |` / `run: >` 块的正文，返回 [(块内首行行号, 正文)]。"""
    lines = text.split("\n")
    blocks = []
    i = 0
    while i < len(lines):
        m = RE_RUN_HEAD.match(lines[i])
        if not m:
            i += 1
            continue
        base = len(m.group(1))
        j = i + 1
        body = []
        while j < len(lines):
            line = lines[j]
            if line.strip() == "":
                body.append(line)
                j += 1
                continue
            if len(line) - len(line.lstrip(" ")) <= base:
                break
            body.append(line)
            j += 1
        # 剥掉块体公共缩进，得到可直接喂给 bash 的脚本
        nonempty = [l for l in body if l.strip()]
        if nonempty:
            cut = min(len(l) - len(l.lstrip(" ")) for l in nonempty)
            body = [l[cut:] if l.strip() else "" for l in body]
        blocks.append((i + 2, "\n".join(body)))
        i = j
    return blocks


def check_shell(normalized):
    print("[2/5] 每个 run 块的 shell 语法（bash -n）")
    if not normalized:
        return
    blocks = extract_run_blocks(normalized)
    if not blocks:
        bad("没能解析出任何 run 块 —— 解析器与文件结构脱节了，这一项等于没跑")
        return
    broken = 0
    for start, body in blocks:
        script = re.sub(r"\$\{\{.*?\}\}", "DUMMY", body, flags=re.S)
        try:
            r = subprocess.run(
                [BASH, "-n"],
                input=script,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
            )
        except FileNotFoundError:
            bad("找不到 bash 可执行文件 %r（Windows 本机请设 CHECK_BASH 指向 Git Bash）" % BASH)
            return
        if r.returncode != 0:
            err = (r.stderr or "").strip().splitlines()
            detail = err[0] if err else "exit %d" % r.returncode
            bad(
                "%s:%d 起的 run 块 shell 语法错（bash -n）：%s"
                % (show(WORKFLOW), start, detail)
            )
            broken += 1
    if not broken:
        ok("%d 个 run 块语法均可解析" % len(blocks))


# --------------------------------------------------------------------------- #
# 3. 版本号
# --------------------------------------------------------------------------- #

RE_VERSION = re.compile(r'^version\s*=\s*"([^"]*)"', re.M)
RE_SAFE_VERSION = re.compile(r"[0-9A-Za-z][0-9A-Za-z.+-]*")


def check_version():
    print("[3/5] 版本号解析与字符集")
    if not CARGO_TOML.exists():
        bad("找不到 %s，无法校验版本号" % show(CARGO_TOML))
        return
    txt = CARGO_TOML.read_text(encoding="utf-8", errors="replace")
    m = RE_VERSION.search(txt)
    if not m:
        bad(
            "%s 里取不到版本号（找不到 ^version = \"...\" 行）—— "
            "workflow 的 Resolve version 会拿到空串，产物目录名 / 压缩包名随之塌掉"
            % show(CARGO_TOML)
        )
        return
    ver = m.group(1)
    if not RE_SAFE_VERSION.fullmatch(ver):
        bad(
            "%s 版本号 %r 含文件名非法字符（只允许 0-9 A-Z a-z . + -）—— "
            "它会直接进产物目录名与压缩包名，upload-artifact 会拒收"
            % (show(CARGO_TOML), ver)
        )
    else:
        ok("版本号 " + ver)


# --------------------------------------------------------------------------- #
# 4. steps.<id>.outputs.<key> 引用
# --------------------------------------------------------------------------- #

RE_OUTPUT_REF = re.compile(r"steps\.([A-Za-z0-9_-]+)\.outputs\.([A-Za-z0-9_-]+)")
RE_LIST_ITEM = re.compile(r"^(\s*)-\s+")


def collect_steps(lines):
    """按 steps 列表项切分，返回 {step_id: 该 step 的正文}。

    以「最浅的 `- ` 缩进」作为 step 边界（矩阵 include 之类的嵌套列表缩进更深，
    不会被误判成 step）。id 两种写法都认：`- id: x` 与 `- name: x` 换行后 `id: x`。
    """
    dashes = [len(m.group(1)) for m in (RE_LIST_ITEM.match(l) for l in lines) if m]
    if not dashes:
        return {}
    step_indent = min(dashes)

    items, cur = [], None
    for line in lines:
        m = RE_LIST_ITEM.match(line)
        if m and len(m.group(1)) == step_indent:
            if cur is not None:
                items.append(cur)
            cur = [line]
            continue
        if cur is not None:
            cur.append(line)
    if cur is not None:
        items.append(cur)

    steps = {}
    for item in items:
        body = "\n".join(item)
        m = re.search(r"^\s*(?:-\s+)?id:\s*([A-Za-z0-9_-]+)\s*$", body, re.M)
        if m:
            steps.setdefault(m.group(1), body)
    return steps


def check_outputs(normalized):
    print("[4/5] steps.<id>.outputs.<key> 引用是否有出处")
    if not normalized:
        return
    steps = collect_steps(normalized.split("\n"))
    refs = sorted(set(RE_OUTPUT_REF.findall(normalized)))
    if not refs:
        ok("没有被引用的 step 输出")
        return
    missing = 0
    for sid, key in refs:
        ref = "steps.%s.outputs.%s" % (sid, key)
        if sid not in steps:
            bad(
                "workflow 引用了 %s，但没有任何 `- id: %s` 的 step —— "
                "GitHub 会把它求值成空串" % (ref, sid)
            )
            missing += 1
            continue
        if not re.search(
            r"(?m)(^|[^A-Za-z0-9_-])%s=(?!=)|%s<<"
            % (re.escape(key), re.escape(key)),
            steps[sid],
        ):
            bad(
                "%s 没有出处：id 为 %s 的 step 里没有任何地方往 $GITHUB_OUTPUT 写 %s —— "
                "GitHub 对这种引用**求值成空串且不报错**，最后会表现为"
                "「产物名 / 目录名莫名塌掉」"
                % (ref, sid, key)
            )
            missing += 1
    if not missing:
        ok("%d 处 step 输出引用都有出处" % len(refs))


# --------------------------------------------------------------------------- #
# 5. `$VAR` 后紧跟非 ASCII 字符（macOS bash 3.2 会把标点吃进变量名）
# --------------------------------------------------------------------------- #

RE_BARE_VAR = re.compile(r"\$([A-Za-z_][A-Za-z0-9_]*)")


def check_var_cjk(normalized):
    """扫「`$VAR` 后面紧跟非 ASCII 字符」的写法。

    为什么必须查：macOS runner 的 `/bin/bash` 是 3.2，变量名解析按**字节**走
    `isalnum`，而 Apple 的 libc 在 UTF-8 环境下把全角标点的前导字节也算作字母 ——
    于是 `$VERSION（` 变成变量 `VERSION（`，在 `set -u` 下直接
    `VERSION?: unbound variable`。Linux 的 bash 5 + glibc 不这样，所以同一个脚本
    只有 macOS job 红，报错里的变量名还带着一个乱码字节，很难第一时间想到是标点。
    写成 `${VERSION}（` 就永远不会有歧义。

    范围：workflow 正文 + `scripts/*.sh`（打包脚本不在 workflow 里，只在仓库里，
    但它是同一个 bash 在执行，同样会炸）。
    """
    print("[5/5] 变量名后紧跟非 ASCII 字符")
    if not normalized:
        return
    targets = [(show(WORKFLOW), normalized)]
    for path in sorted((REPO / "scripts").glob("*.sh")):
        try:
            text = path.read_bytes().decode("utf-8", "replace").replace("\r\n", "\n")
        except OSError as exc:
            bad("读不到 %s：%s" % (show(path), exc))
            continue
        targets.append((show(path), text))

    hits = 0
    for name, text in targets:
        for lineno, line in enumerate(text.split("\n"), 1):
            # 注释不执行；本文件的说明里会出现反面例子（`$VERSION（`），不能误报
            if line.strip().startswith("#"):
                continue
            for m in RE_BARE_VAR.finditer(line):
                nxt = line[m.end() : m.end() + 1]
                if nxt and ord(nxt) > 127:
                    bad(
                        "%s:%d 里 `$%s` 后面紧跟 `%s` —— macOS 的 bash 3.2 会把它算进"
                        "变量名，`set -u` 下报 unbound variable；改写成 `${%s}%s`"
                        % (name, lineno, m.group(1), nxt, m.group(1), nxt)
                    )
                    hits += 1
    if not hits:
        ok("%d 个文件里没有「$VAR 紧跟非 ASCII」的写法" % len(targets))


# --------------------------------------------------------------------------- #


def main():
    print("预检 " + show(WORKFLOW))
    try:
        raw = WORKFLOW.read_bytes()
    except OSError as exc:
        bad("读不到 %s：%s" % (show(WORKFLOW), exc))
        return 1

    normalized = check_invisible(raw)
    if normalized:
        check_shell(normalized)
        check_version()
        check_outputs(normalized)
        check_var_cjk(normalized)

    if PROBLEMS:
        print("\n预检未通过：%d 个问题（上面 [!!] 各行）" % len(PROBLEMS))
        return 1
    print("\n预检通过")
    return 0


if __name__ == "__main__":
    sys.exit(main())
