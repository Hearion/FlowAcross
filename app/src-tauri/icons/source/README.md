# 图标源文件

所有图标都从这里的 SVG 生成，改图标时改 SVG 再重新生成，不要直接改 PNG。

- `app-icon.svg`：应用图标。1024 画布，圆角方块 824（macOS 图标网格），白底纯黑的一笔双环。
- `social.svg`：官网分享图（`FlowAcross-releases/docs/assets/logo-social.png`），满版白底。
- `tray-template.svg`：macOS 菜单栏图标，只有黑色和透明，作为模板图标由系统按菜单栏深浅反色。

在 `FlowAcross/` 目录下：

```bash
app/node_modules/.bin/tauri icon app/src-tauri/icons/source/app-icon.svg -o <临时目录>
```

生成后只把 `app/src-tauri/icons/` 里原有的文件名复制回来（不要 `android/`、`ios/`）。其余几个：

```bash
app/node_modules/.bin/tauri icon app/src-tauri/icons/source/app-icon.svg -o <临时目录> -p 128 -p 32
app/node_modules/.bin/tauri icon app/src-tauri/icons/source/social.svg -o <临时目录> -p 1254
app/node_modules/.bin/tauri icon app/src-tauri/icons/source/tray-template.svg -o <临时目录> -p 64
```

- `128x128.png` → `app/src/assets/logo.png`（界面里的标志）和官网的 `docs/assets/logo.png`
- `32x32.png` → 官网的 `docs/assets/favicon.png`
- `1254x1254.png` → 官网的 `docs/assets/logo-social.png`
- `64x64.png`（菜单栏）→ `app/src-tauri/icons/tray-template.png`
