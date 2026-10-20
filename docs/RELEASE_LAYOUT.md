# Release layout rule

This is the **canonical layout** for a CapLite release folder. Keep it stable across
packaging changes.

## Root

```text
CapLite-1.0.3/
├─ CapLite.exe
├─ README.md
├─ runtime/
│  └─ 7 runtime DLLs
├─ docs/
└─ licenses/
```

- `CapLite.exe` stays at the root.
- All **7 runtime DLLs live in `runtime/`**. The executable registers this directory
  with `AddDllDirectory` before any FFmpeg/OpenH264 load, and the release OpenH264
  loader is explicitly allowed to look there.
- **Do not** put DLLs in `includes/` or in another random subfolder. `runtime/` is
  the one supported name.
- `README.md` is the root entry point.

## `runtime/`

```text
runtime/
├─ avcodec-63.dll
├─ avformat-63.dll
├─ avutil-61.dll
├─ swresample-7.dll
├─ swscale-10.dll
├─ openh264.dll
└─ openh264-7.dll
```

These are the only files allowed in `runtime/`.

## `docs/`

```text
docs/
├─ README.txt
└─ screenshots/
   ├─ record.jpg
   ├─ trim.jpg
   └─ settings.jpg
```

## `licenses/`

```text
licenses/
├─ LICENSE
├─ LICENSE-FFmpeg.txt
├─ LICENSE-OpenH264.txt
└─ THIRD-PARTY-NOTICES.txt
```

## Packaging / verification

1. Build `target/release/CapLite.exe` and sign it.
2. Copy `CapLite.exe` to the release root.
3. Copy the 7 DLLs to `runtime/`.
4. Keep `README.md` at the root; keep `docs/` and `licenses/`.
5. Run:

```powershell
pwsh -NoProfile -File scripts\check-release-layout.ps1
```

6. Rebuild the ZIP from the `CapLite-1.0.3` folder; the ZIP root must be
   `CapLite-1.0.3/`.

The loader rule is the reason this layout is fixed: `AddDllDirectory(<exe>\runtime)`
runs before delay-loaded FFmpeg and before the OpenH264 preload.
