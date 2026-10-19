"""从 FFmpeg 的 **DLL 导出表**生成 **MSVC 格式的导入库**（.def + lib.exe）。

# 为什么需要它

`ffmpeg-sys-next` 链接的是随 FFmpeg 分发包一起来的 `*.lib`。实测（本机 MSVC 14.51）：

* 那些 `.lib` 是 **MinGW/GNU 风格**的导入库（lib.exe /dumpbin 里成员名形如
  `libavcodec_avcodec_lib`）；
* 用它们链接出的 exe **能正常运行**，但 `link.exe` **认不出"这些导入属于哪个 DLL"**：
  `/DELAYLOAD:avcodec-63.dll` 会被判成 `LNK4199 已忽略；未找到来自 avcodec-63.dll 的导入`，
  于是 **Delay Import Directory 是空的 → 延迟加载静默失效**（照样隐式映射 87 MB 的
  avcodec-63.dll）。
* 同样一条 `/DELAYLOAD`，若链接的是 **MSVC 自己生成的导入库**（`lib.exe /def:`），
  则完全正常。最小复现（`_rust/tmp/rustdl`）已验证：`probe.lib`(MSVC) 成功、
  `avcodec.lib`(MinGW) 失败。

# 做法

1. `dumpbin /exports <dll>` 取出全部导出符号名；
2. 写成 `.def`（只列名字，不写序号：FFmpeg 的导出序号从 1 开始且**不连续**，
   带序号的 def 反而容易错位）；
3. `lib.exe /machine:x64 /def:x.def /out:x.lib` 生成 MSVC 导入库；
4. 放进一个独立目录，`build.rs` 用 `rustc-link-search` 把这个目录排在
   `FFMPEG_DIR/lib` **前面**，链接器就会优先用 MSVC 版导入库。

# 用法

    python scripts/make-msvc-import-libs.py --ffmpeg-dir <FFMPEG_DIR> --out <目录>

脚本是幂等的：`.def`/`.lib` 已存在且 DLL 未变（比较 mtime）就跳过。
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

# 需要转换的 5 个 LGPL 库（与 build.rs / Cargo.toml 的 feature 保持一致）
LIBS = ["avcodec", "avformat", "avutil", "swresample", "swscale"]

EXPORT_LINE = re.compile(r"^\s+\d+\s+[0-9A-Fa-f]+\s+[0-9A-Fa-f]{8}\s+(\S+)\s*$")


def find_tool(name: str, roots: list[Path]) -> Path | None:
    for root in roots:
        if not root.is_dir():
            continue
        for hit in root.rglob(name):
            return hit
    return None


def msvc_tools() -> tuple[Path, Path]:
    """定位 `dumpbin.exe` 与 `lib.exe`（Hostx64\\x64 优先）。"""
    import os

    roots: list[Path] = []
    if os.environ.get("VCToolsInstallDir"):
        roots.append(Path(os.environ["VCToolsInstallDir"]))
    prog = Path(r"C:\Program Files\Microsoft Visual Studio")
    if prog.is_dir():
        roots.extend(sorted(prog.glob(r"*\*\VC\Tools\MSVC\*")))
    prog86 = Path(r"C:\Program Files (x86)\Microsoft Visual Studio")
    if prog86.is_dir():
        roots.extend(sorted(prog86.glob(r"*\*\VC\Tools\MSVC\*")))
    if not roots:
        raise SystemExit("找不到 MSVC 工具集目录（设 VCToolsInstallDir 或装 VS Build Tools）")

    used = [r for r in roots if (r / "bin" / "Hostx64" / "x64" / "dumpbin.exe").is_file()]
    if not used:
        raise SystemExit(f"在 {roots} 里找不到 bin\\Hostx64\\x64\\dumpbin.exe")
    bin_dir = used[0] / "bin" / "Hostx64" / "x64"
    return bin_dir / "dumpbin.exe", bin_dir / "lib.exe"


def exports_of(dumpbin: Path, dll: Path) -> list[str]:
    out = subprocess.run(
        [str(dumpbin), "/exports", str(dll)],
        capture_output=True,
        text=True,
        errors="replace",
    )
    if out.returncode != 0:
        raise SystemExit(f"dumpbin 失败（{dll}）：\n{out.stdout}\n{out.stderr}")
    names: list[str] = []
    for line in out.stdout.splitlines():
        m = EXPORT_LINE.match(line)
        if m:
            names.append(m.group(1))
    if not names:
        raise SystemExit(f"{dll} 的导出表里没解析出任何符号名")
    return names


def convert(dumpbin: Path, lib_exe: Path, dll: Path, out_dir: Path) -> Path:
    stem = dll.stem  # avcodec-63
    base = stem.split("-")[0]  # avcodec
    def_file = out_dir / f"{base}.def"
    lib_file = out_dir / f"{base}.lib"

    if lib_file.exists() and lib_file.stat().st_mtime >= dll.stat().st_mtime:
        print(f"[skip] {lib_file.name} 比 {dll.name} 新")
        return lib_file

    names = exports_of(dumpbin, dll)
    def_file.write_text(
        "LIBRARY %s\nEXPORTS\n%s\n" % (dll.name, "\n".join(f"    {n}" for n in names)),
        encoding="ascii",
    )
    res = subprocess.run(
        [str(lib_exe), "/nologo", "/machine:x64", f"/def:{def_file}", f"/out:{lib_file}"],
        capture_output=True,
        text=True,
        errors="replace",
    )
    if res.returncode != 0 or not lib_file.is_file():
        raise SystemExit(f"lib.exe 失败（{base}）：\n{res.stdout}\n{res.stderr}")
    print(f"[ok]   {lib_file.name} ← {dll.name}（{len(names)} 个导出）")
    return lib_file


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--ffmpeg-dir", required=True, help="FFMPEG_DIR（其下应有 bin/ 与 lib/）")
    ap.add_argument("--out", required=True, help="生成的 MSVC 导入库放这里")
    ap.add_argument("--libs", nargs="*", default=LIBS)
    args = ap.parse_args()

    ffmpeg = Path(args.ffmpeg_dir)
    out_dir = Path(args.out)
    out_dir.mkdir(parents=True, exist_ok=True)

    dumpbin, lib_exe = msvc_tools()
    print(f"dumpbin : {dumpbin}")
    print(f"lib.exe : {lib_exe}")

    made = 0
    for base in args.libs:
        candidates = sorted((ffmpeg / "bin").glob(f"{base}-*.dll"))
        if not candidates:
            print(f"[warn] {ffmpeg / 'bin'} 下没有 {base}-*.dll，跳过")
            continue
        before = (out_dir / f"{base}.lib").exists()
        convert(dumpbin, lib_exe, candidates[0], out_dir)
        if not before:
            made += 1
    print(f"[done] 转换完成，新生成 {made} 个导入库 → {out_dir}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
