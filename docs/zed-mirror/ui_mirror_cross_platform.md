# Zed UI 远程镜像:跨平台场景(Linux 宿主 → macOS 客户端)

> 前置文档:`ui_mirror_plan_c.md`(方案 C,GPUI 绘制指令流)。调研方式:对源码只读调研(分支 `ymzed`,2026-10-16)。
> 新场景:宿主机正常运行桌面版 Zed,作为 server;macOS 上运行一个客户端,镜像 server 的画面并可以操作。宿主和客户端是两套不同的系统(下文以 Linux x86_64 宿主、macOS arm64 客户端为主,Windows 宿主的差异在第 6 节)。

## 1. 结论

**可行。方案 C 本来就和平台无关,换成异构系统也不需要改架构。** 原因是宿主只往外发"已经光栅化好的位图加上绘制图元",客户端不需要宿主的字体、文本系统、keymap 或任何平台 API。

需要额外处理的问题有 5 个,都在 mirror crate 和客户端里解决,不用改 GPUI:

| # | 问题 | 处理方式 |
|---|---|---|
| 1 | 客户端如果用 Metal 渲染器,遇到子像素文字会直接 panic | macOS 客户端改用 `gpui_wgpu`(wgpu 的 Metal 后端),或者宿主关闭子像素 |
| 2 | Metal 和 wgpu 的文字着色器不同,文字粗细会不一样 | 同上,用 wgpu,和 Linux 宿主走同一套 WGSL |
| 3 | 修饰键语义不同:Mac 的 Cmd 到了 Linux 是 Super | 客户端把 Cmd 映射成 Ctrl |
| 4 | Linux 宿主通常是 1x 缩放,在 Retina 屏上会发虚 | C1 下调高宿主缩放;C2 虚拟窗口直接按客户端的 2x 出图 |
| 5 | Linux 的客户端侧窗口装饰(标题栏的关闭/最小化按钮)也会画进 Scene | 在包装层拦截或者忽略这些按钮的效果 |

**推荐路线:** 直接做 C2 虚拟窗口,窗口尺寸用桌面尺寸。Mac 是桌面设备,不需要"手机形态";C2 能同时解决清晰度(问题 4)、不可见时不出帧(原文第 7 节第 1 条,Wayland 下更严重)和装饰按钮(问题 5)三个问题。

## 2. 为什么不同系统不影响核心链路

| 环节 | 跨平台情况 | 依据 |
|---|---|---|
| 文字/SVG/图片光栅化 | 全在宿主上完成,客户端只收位图 | `AtlasKey::{Glyph,Svg,Image}`,`crates/gpui/src/platform.rs:1699` |
| atlas 像素格式 | 单色:1 字节覆盖率(Metal `A8Unorm`,wgpu `R8Unorm`,数据一样);彩色:都是 BGRA8 | `crates/gpui_apple/src/metal_atlas.rs:146,150`,`crates/gpui_wgpu/src/wgpu_atlas.rs:200`;cosmic-text 光栅化后会把 RGBA 转成 BGRA,`crates/gpui_wgpu/src/cosmic_text_system.rs:494` |
| Scene 图元 | 字段都是公开的普通数值类型(`ScaledPixels`、`Hsla`、`Bounds` 等) | `crates/gpui/src/scene.rs` |
| CPU 架构 / 字节序 | x86_64 和 arm64 都是小端、64 位 | 但仍然**不要**直接 memcpy `#[repr(C)]` 结构体,见第 4 节 |
| 输入 | `PlatformInput` 是平台无关的枚举 | `crates/gpui/src/interactive.rs:771` |
| 客户端渲染器 | `gpui_wgpu` 在 macOS 上能编译(wgpu 默认特性带 Metal 后端) | 本机已验证 `cargo check -p gpui_wgpu` 通过 |

反过来,有一点比 macOS 宿主**更好**:Linux 和 Windows 上 Zed 的应用菜单是画在窗口里的(`ApplicationMenu`,`crates/title_bar/src/title_bar.rs:465-477`),所以菜单会被镜像过去,不会像 macOS 宿主那样丢掉系统菜单栏。

## 3. 需要处理的差异(详细)

### 3.1 子像素文字和渲染器选择(必须处理)

- Linux 的 X11/Wayland 窗口在 GPU 支持时会报告 `is_subpixel_rendering_supported() == true`(`crates/gpui_linux/src/linux/x11/window.rs:1635`、`wayland/window.rs:1918`),GPUI 就会产生 `SubpixelSprite`(`crates/gpui/src/window.rs:4899`)。
- macOS 的 Metal 渲染器**不支持**子像素:`metal_atlas.rs` 中 `AtlasTextureKind::Subpixel => unreachable!()`,`metal_renderer.rs:795` 中 `PrimitiveBatch::SubpixelSprites { .. } => unreachable!()`。拿 Linux 宿主的帧直接喂给 Metal 渲染器会 panic。
- 另外,WGSL 着色器会对单色字形做 gamma 和对比度校正(`get_gamma_correction_ratios`、`grayscale_enhanced_contrast`,`crates/gpui_wgpu/src/wgpu_renderer.rs:92-94`),而 Metal 的 `monochrome_sprite_fragment` 只是 `color.a *= sample.a`(`crates/gpui_apple/src/shaders.metal:659`)。同一份覆盖率位图在两个渲染器上看起来粗细不同。

**做法:** macOS 客户端使用 `WgpuRenderer`(底层是 Metal),和 Linux 宿主走同一套着色器,画面基本一致,也支持子像素。这样 macOS 客户端和原文推荐的 PWA、Android/Linux 客户端是同一份渲染代码。

如果坚持用 `gpui_apple` 的 Metal 渲染器,就让 `MirrorWindow::is_subpixel_rendering_supported()` 返回 `false`。代价是宿主本地的文字也会变成灰度抗锯齿。

### 3.2 修饰键和 keymap(必须处理)

- keymap 是在宿主上解析的,Linux 宿主加载的是 `keymaps/default-linux.json`(`crates/settings/src/settings.rs:167`),绑定基本都是 `ctrl-*`。
- `Modifiers::platform` 在 Mac 上是 Cmd,在 Linux 上是 Super;`Keystroke::parse` 把 `cmd`、`super`、`win` 都解析成 `platform`(`crates/gpui/src/platform/keystroke.rs:152`)。Mac 用户按 Cmd-S,宿主收到的是 Super-S,没有绑定。
- **做法:** 在客户端加一个可切换的映射,默认 Cmd → Ctrl。Mac 上原本的 Ctrl(Emacs 风格的行内移动)可以映射成 Ctrl,或者直接丢弃,做成选项。
- 不建议在宿主上改用 macOS 的 keymap:keymap 是 `App` 级别的全局状态,会影响宿主本地的使用。
- `Keystroke::key_char`(例如 Option 组合出的特殊字符)由客户端按 Mac 的键盘布局算好再发送,宿主直接用,不需要知道 Mac 的布局。IME 依旧走原文 5.1 节的 `ImeUpdate`/`ImeCommit`,客户端用 `NSTextInputClient` 组字。

### 3.3 缩放比例和清晰度

- Linux 桌面经常是 1x(或者 Wayland 的分数缩放),Mac 是 2x。C1 镜像下 sprite 按宿主缩放比例光栅化,放到 Retina 上放大 2 倍,文字明显发虚。
- C1 下的缓解办法:提高宿主本身的缩放比例(X11 可以设 `GPUI_X11_SCALE_FACTOR`,见 `crates/gpui_linux/src/linux/x11/client.rs:80`;Wayland 调整输出缩放)。**不能**只让 `MirrorWindow::scale_factor()` 报告 2.0,因为内层原生窗口的 surface 还是 1x,宿主本地画面会错乱。
- C2 虚拟窗口直接用客户端上报的 2.0,cosmic-text 按 2x 光栅化,在 Mac 上是原生清晰度。

### 3.4 窗口装饰和窗口级操作

- Linux 上 Zed 会画客户端侧装饰(`Decorations::Client`,`crates/workspace/src/workspace.rs:11959-12031`),标题栏的关闭、最小化、最大化按钮和拖动区域都在 Scene 里。Mac 用户点这些按钮,实际操作的是宿主的原生窗口。
- C1:在 `MirrorWindow` 里让来自远端的 `minimize`、`zoom`、`start_window_move`、`start_window_resize` 变成空操作;关闭按钮最好也拦截掉。
- C2:`RemoteWindow::window_decorations()` 返回 `Decorations::Server`,这样 workspace 不会画装饰阴影和缩放边框。标题栏的风格由编译期的 `PlatformStyle::platform()` 决定,仍然是 Linux 风格,但这只是外观问题。

### 3.5 剪贴板、文件对话框、路径

- 剪贴板:在 Mac 上复制,操作的是宿主(X11/Wayland)的剪贴板。需要双向同步,把文本和图片放进协议的 `Clipboard` 消息。
- `use_system_path_prompts: false` 依然必要。另外,路径都是**宿主的 Linux 路径**,Mac 的 Finder 拖放文件不能直接对应,首版不支持拖放。
- 系统通知、`open_url`、`reveal_path` 这类调用发生在宿主上,Mac 用户看不到。需要的话在 `MirrorPlatform` 里转发 `open_url`,其余忽略。

### 3.6 帧驱动(Wayland 特有)

Wayland 合成器会对不可见或最小化的 surface 停发 frame callback,所以宿主窗口被最小化或切到别的工作区时,C1 会直接停帧。比原文第 7 节第 1 条(macOS 锁屏)更容易遇到。C2 自己控制出帧节奏,不受影响。

## 4. 协议上的注意事项

1. **显式序列化,不要传内存布局。** 虽然两端都是小端、64 位,但:
   - `PaintSurface` 的字段受 `cfg` 控制(`crates/gpui/src/scene.rs:772`),Linux 宿主根本不会产生它;
   - `FontId(usize)`、`ImageId(usize)` 这类类型的大小取决于平台;
   - 将来如果有 wasm32 客户端,`usize` 只有 4 字节。
   在 mirror crate 里为每种图元定义线上格式(serde 加 postcard/bincode,或者手写),宿主端从 `Scene` 转换过去,客户端再转换回来。
2. **握手时带上协议版本和宿主的 `git sha`。** 两端最好用同一个 commit 编译,版本不一致时拒绝连接,避免 `Scene` 字段变化导致解码错位。
3. **tile 位图上加上格式标签**(`R8` / `BGRA8`,是否预乘 alpha),不要让客户端根据 `AtlasTextureKind` 去猜。以后接入 Windows 宿主或其他文本系统时不容易出错。

## 5. 客户端形态(macOS)

| 形态 | 说明 | 评价 |
|---|---|---|
| **原生小程序:winit + `WgpuRenderer::new`** | 用 `raw-window-handle` 建 surface,走 Metal 后端;键盘用 winit 事件,IME 用 winit 的 `Ime` 事件 | **首选**。和宿主同一套着色器,支持子像素,代码量小 |
| 浏览器 PWA(Safari/Chrome) | 原文 M3 的同一份客户端 | 零安装,但 Safari 里很多 Cmd 组合键会被浏览器先吃掉,不适合重度编辑 |
| `gpui_apple` Metal 渲染器加 AppKit 壳 | 需要关掉宿主的子像素,文字粗细和宿主不同 | 不推荐 |

## 6. Windows 宿主的差异(备注)

- `gpui_windows` 的窗口**始终**报告支持子像素(`crates/gpui_windows/src/window.rs:906`),ClearType 默认开启。所以 3.1 节的问题更突出,客户端必须能画 `SubpixelSprite`(用 wgpu)。
- Windows 也画客户端侧标题栏和应用菜单,处理方法同 3.4 节;修饰键映射同 3.2 节。
- DirectWrite 的光栅化结果同样是 1 字节覆盖率或 BGRA,不影响协议,但要注意 3.1 节说的 gamma 差异:DirectX 着色器和 WGSL 的参数可能不同。如果客户端用 wgpu 渲染,文字和宿主本地会有细微差别,可以接受。

## 7. 对里程碑的影响

在原文第 8 节的基础上:

- M1 的"桌面原生 wgpu 测试客户端"直接在 macOS 上做,本身就能验证跨平台链路,不增加工作量。
- 新增**约 3–5 天**:修饰键映射、拦截装饰按钮、剪贴板双向同步、协议版本握手。
- 如果 Mac 是主要使用场景,建议把 M5(C2 虚拟窗口)提前到 M4 之前,并去掉 M4 的"手机形态"UI 调整。

## 8. 变体 C3:在 macOS 端光栅化(仍然不改 GPUI 核心)

### 8.1 能做到哪一层

| 层次 | 在哪里做 | 能否搬到 macOS |
|---|---|---|
| App 状态、元素树、布局、文本整形(换行、字形位置) | 宿主 | **不能**。布局必须和宿主状态同步计算,搬到 Mac 就等于在 Mac 上运行完整的 Zed(即现有的远程开发或协作模式),已经不是"镜像"了 |
| 矢量图元(Quad、Shadow、Underline、Path) | 宿主只生成数据 | 本来就在客户端绘制 |
| **文字光栅化** | 原方案在宿主 | **能**。只转发字形引用,Mac 用 CoreText 光栅化 |
| **SVG 图标光栅化** | 原方案在宿主 | **能**。Mac 客户端自带 Zed 的 assets,用路径和尺寸自己渲染 |
| 图片 | 宿主解码 | 原图只发一次,和缩放比例无关,保持现状 |

结论:能做到"布局在 Linux,所有像素在 Mac"。最终画面是 Mac 原生的 CoreText 字形和 Metal 着色器,字形位置和换行由宿主决定。

### 8.2 关键接口(全部是公开的)

- `Platform::text_system() -> Arc<dyn PlatformTextSystem>`(`crates/gpui/src/platform.rs:366`):`MirrorPlatform` 可以再包一层 `MirrorTextSystem`,截获:
  - `add_fonts(bytes)`:Zed 内置字体的原始字节;
  - `font_id(&Font)`:宿主 `FontId` 和字体描述的对应关系;
  - `glyph_raster_bounds(params)`:宿主光栅化边界,用来在客户端反推字形原点。
- 字形 sprite 的位置是这样算的:`bounds.origin = integer_origin + raster_bounds.origin`(`crates/gpui/src/window.rs:4859-4870`)。客户端用 `sprite.origin - 宿主raster_origin` 得到字形原点,再加上 CoreText 自己算出的 `raster_bounds.origin`。两边光栅化边界差一两个像素也没关系。
- `RenderGlyphParams` 的字段全部公开(`font_id, glyph_id, font_size, subpixel_variant, scale_factor, is_emoji, subpixel_rendering, dilation`,`crates/gpui/src/text_system.rs:1264`)。
- 客户端:`gpui_apple::AppleTextSystem`(公开,`crates/gpui_apple/src/gpui_apple.rs:21`)负责 `add_fonts`、`font_id` 和 `rasterize_glyph`;`gpui_apple::metal_renderer::MetalRenderer`(公开,带 `new`/`draw`/`sprite_atlas`)负责绘制。因为光栅化在 Mac 上做,客户端统一把 `subpixel_rendering` 设为 false,第 3.1 节"Metal 不支持子像素"的问题也就不存在了。`dilation` 可以由客户端根据颜色用 `glyph_dilation_for_color` 重新计算,得到 Mac 原生的字重表现。

### 8.3 协议变化

atlas 下行消息从"位图"改成"引用",保留位图作为兜底:

- `FontAdded{host_font_id, hash, bytes}`:按 hash 在客户端缓存,同一个字体只传一次。
- `TileAdded{host_tile_id, source}`,其中 `source` 是下面三种之一:
  - `Glyph{host_font_id, glyph_id, font_size, subpixel_variant, is_emoji, host_raster_origin}`
  - `Svg{path, size}`
  - `Bitmap{format, size, bytes}`:兜底,用于图片、彩色 emoji 和拿不到字体文件的字形。

### 8.4 前提和难点

1. **两边必须是同一个字体文件。** `GlyphId` 是字体文件内部的字形序号,只有用同一份字体文件才对得上,所以不能让 Mac 按名字去找"同名字体"。宿主要把字体字节发给客户端。
2. **系统字体和回退字体拿不到字节。** Zed 内置字体经过 `add_fonts`,能截获到字节;但 Linux 上的系统字体和整形时自动选中的回退字体(**中文一定会走到这里**,例如 Noto Sans CJK),是在 `CosmicTextSystem` 内部加载的,`PlatformTextSystem` 接口没有暴露字体数据。两种解决办法:
   - 在 `gpui_wgpu::CosmicTextSystem` 上加一个很小的公开访问器,例如 `font_data(FontId)`。只改 `gpui_wgpu`,不改 `gpui` 核心;
   - 或者把这些字形降级成 `Bitmap` 兜底。
   CJK 字体每个字重 15–20MB,首次连接要传一次,之后按 hash 缓存。
3. **彩色 emoji:** Linux 的 Noto Color Emoji 是 CBDT 格式,CoreText 对 CBDT 的支持没有把握,建议直接走 `Bitmap` 兜底。
4. **渲染效果差异:** cosmic-text 在缺少某个字重时会合成粗体或斜体,CoreText 不会。同一个字形在两边看起来可能有细微差别,但不影响位置。
5. **字形位置不会变:** 字形前进宽度来自同一份字体文件的 `hmtx` 表,所以两边一致,换行和光标位置不会错位。

### 8.5 额外收益

- **清晰度和宿主的缩放比例无关。** 客户端把所有 `ScaledPixels` 乘以 `client_scale / host_scale`,字形用 Mac 的缩放比例和对应的 `subpixel_variant` 重新光栅化,SVG 用 `size × k` 渲染。所以 **C1 镜像模式下也能在 Retina 屏上得到清晰的文字**,第 3.3 节的问题在不做 C2 的情况下也解决了。只有布局还按宿主的像素网格对齐,这在视觉上几乎看不出来。
- **带宽更小。** 一个字形引用大约 20 字节,而一个字形位图是几百到几千字节。
- **画面是 Mac 原生风格**:CoreText 抗锯齿,加上 Metal 着色器。

### 8.6 建议

C3 可以和 C1、C2 叠加,因为它只改了 atlas 这一路。建议在 M1 打通 Bitmap 链路之后实现,作为 M2.5,工作量约 1–1.5 周,其中字体字节获取和缓存约占一半。
