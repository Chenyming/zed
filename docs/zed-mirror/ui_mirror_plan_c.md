# Zed UI 远程镜像:方案 C(GPUI 绘制指令流)结论

> 调研方式:对 Zed 源码只读调研(分支 `ymzed`,2026-10-16)。
> 目标:在家里电脑运行 Zed,外出时用手机、iPad、Android 或 Linux 客户端连上它并操作,例如在 agent 线程里输入消息,形式上类似结对编程。客户端不需要实现编辑器功能。
> 已放弃的方案:A(系统级远程桌面)、B(像素推流)。另一个保留方案是 D(agent 语义层 API),见文末。

## 1. 结论

- **方案 C 可行,而且不用修改 GPUI。** 所有改动都在 Zed 这一侧:新增一个 crate,再改 `crates/zed/src/main.rs:88` 的一行。
- 宿主以**手机形态**显示,客户端 1:1 镜像。这是起步最简单的形态。
- 以后可以升级为**虚拟窗口**:宿主桌面保持正常布局,手机拥有自己的窗口,两者共享同一个 `App` 的状态。这一步同样不用改 GPUI。

## 2. 关键源码事实

| 事实 | 位置 | 意义 |
|---|---|---|
| 平台在 Zed 入口处构造,并作为 `Rc<dyn Platform>` 传给 `Application` | `crates/zed/src/main.rs:88-92`,`crates/gpui_platform/src/gpui_platform.rs:67` | 可以在这里插入一层包装 |
| `PlatformWindow` 是公开的 trait,绘制只有一个入口 `draw(&Scene)`,另有 `sprite_atlas()` | `crates/gpui/src/platform.rs:1193`、`:1276-1278` | 一个窗口的所有输出都经过这里 |
| `PlatformAtlas::get_or_insert_with(key, build)` 和 `remove(key)` 是公开的 | `crates/gpui/src/platform.rs:1743` | 可以截获每个新光栅化出来的 tile 的位图 |
| Scene 图元字段全部是 `pub`;`Scene` 公开了 `batches()`、`insert_primitive()`、`push_layer()`、`finish()` | `crates/gpui/src/scene.rs:41`、`:87-172`、`:535-790` | 宿主可以序列化 Scene,客户端可以重建 Scene |
| 文字、SVG、图片都在宿主上光栅化成 atlas 位图;atlas 键为 `Glyph/Svg/Image` | `crates/gpui/src/platform.rs:1699` | 客户端不需要字体和排版引擎 |
| `FontId(pub usize)`、`GlyphId(pub u32)`、`ImageId(pub usize)` | `crates/gpui/src/text_system.rs:42,1255`,`crates/gpui/src/assets.rs:33` | 客户端可以合成 atlas 键,用来插入远端发来的 tile |
| 输入事件的总入口是 `PlatformInput`,已经包含 Touch、Pinch、LongPress、TouchDrag 等 | `crates/gpui/src/interactive.rs:771` | 远端的触控和键鼠可以无损回放 |
| `PlatformInputHandler` 的 `replace_text_in_range`、`replace_and_mark_text_in_range`、`dispatch_input` 是 `pub` | `crates/gpui/src/platform.rs:1983-2090` | 宿主可以在本地应用远端发来的 IME 预编辑和提交文本 |
| 按键先按快捷键分发,没有被处理才回落到输入处理器插入文本 | `crates/gpui/src/window.rs:5551`(`dispatch_keystroke`) | 远端按键的回放要照搬这个顺序 |
| GPUI 只记录窗口可见状态,是平台层在 `Hidden` 时停止请求帧 | `crates/gpui/src/window.rs:2266`,`crates/gpui/src/platform.rs:106-113` | 包装层可以自己控制出帧 |
| `gpui_wgpu::WgpuRenderer` 公开了 `new_from_canvas`、`new_from_surface`、`draw(&Scene)`、`sprite_atlas()` | `crates/gpui_wgpu/src/wgpu_renderer.rs:351,365,1099,1123` | 浏览器和 Android/Linux 客户端可以直接复用这个渲染器 |
| `gpui_apple` 的 Metal 渲染器支持 `target_os = "ios"` | `crates/gpui_apple/src/gpui_apple.rs:1` | iOS/iPadOS 原生客户端可以复用 |
| `gpui_web` 已经实现浏览器端的 WebGPU/WebGL2、触控、软键盘 IME 镜像和 safe area | `crates/gpui_web/src/{ime_mirror,viewport,events}.rs` | 可以参考;但这些模块是 crate 私有的,不能直接引用 |
| 系统原生对话框可以改成 GPUI 自绘:`use_system_prompts`、`use_system_path_prompts` | `crates/settings_content/src/workspace.rs:93,100` | 必须关掉,否则远端看不到弹窗 |

## 3. 架构:用装饰器包住原生平台

```rust
// crates/zed/src/main.rs
let platform = gpui_platform::current_platform(false);
let platform = ui_mirror::MirrorPlatform::wrap(platform); // 唯一的入口改动
```

| 包装层 | 实现的接口 | 职责 |
|---|---|---|
| `MirrorPlatform` | `Platform` | 其余调用全部转发给原生平台;`open_window` 返回的窗口外再包一层 `MirrorWindow`(或者返回虚拟的 `RemoteWindow`,见第 6 节) |
| `MirrorWindow` | `PlatformWindow` | `draw(&Scene)`:先调用 `inner.draw`,再编码 Scene 推给客户端。`on_input`:保存 GPUI 注册的回调,远端输入直接调用它。`set_input_handler`/`take_input_handler`:借出输入处理器,用于远端文本。可以固定 `content_size`;可以对 GPUI 报告窗口一直可见,并自己定时触发 `on_request_frame` 回调 |
| `MirrorAtlas` | `PlatformAtlas` | 包住 `build` 闭包:每个新 tile 的位图和尺寸,连同返回的 `AtlasTile`,向客户端发送一次;`remove` 时通知客户端删除 |

## 4. 宿主的"手机形态"

1. 把宿主窗口固定为手机逻辑尺寸(例如 430×932 pt),连接时按客户端上报的尺寸重新设定,并拒绝用户拖动改变大小。
2. 适配 Zed UI,只改设置或少量代码:
   - 调大 `ui_font_size`、`buffer_font_size`,方便手指点击;
   - 默认隐藏左右 dock,把 agent 面板放大占满窗口;`agent_ui` 的 `conversation_view`、`thread_view` 里已有窄屏处理,可以作为起点;
   - `use_system_prompts: false`、`use_system_path_prompts: false`。
3. 缩放比例:镜像模式下 sprite 按宿主的缩放比例光栅化(Retina 为 2x),在 3x 手机上要放大 1.5 倍,文字略微发虚但可以接受。想要清晰的 3x,需要第 6 节的虚拟窗口。

## 5. 协议与客户端

### 5.1 协议(WebSocket,加 TLS 和 token,建议只在 Tailscale 内网暴露)

- 下行消息:
  - `AtlasTileAdded{host_tile_id, kind, size, bytes}` 和 `AtlasTileRemoved`
  - `Frame{atlas_epoch, 按 batches() 顺序排列的图元}`,和上一帧做增量,再用 zstd 压缩
  - `CursorStyle`
  - `InputConfig`:当前焦点是否可编辑、软键盘类型,数据来自 `set_text_input_configuration`
- 上行消息:
  - `Input(PlatformInput)`:Touch、Pinch、LongPress、ScrollWheel、KeyDown 等
  - `ImeUpdate{marked_text}` 和 `ImeCommit{text}`:客户端在本地组字,宿主调用 `replace_and_mark_text_in_range` 和 `replace_text_in_range`
  - `Viewport{size, scale}` 和 `Clipboard`
- **IME 说明:** 宿主端调用输入处理器是本地同步执行的,这就绕开了"IME 需要跨网络同步查询"的问题。代价是客户端的 IME 拿不到光标附近的上下文,自动纠错和联想会变弱。

### 5.2 客户端只需要渲染器,不需要 GPUI 的应用框架

- **重建 atlas:** 宿主和客户端的分配器不同,同一个 tile 在两边纹理里的位置也不同。客户端用合成键把收到的位图插入自己的 atlas:
  - 单色字形用 `AtlasKey::Glyph`,把 `font_id` 设为 `FontId(host_tile_id)`,`is_emoji = false`;
  - 彩色图像用 `AtlasKey::Image`;
  - 子像素字形把 `subpixel_rendering` 设为 true。
  然后维护"宿主 tile 到客户端 tile"的映射,绘制前替换每个 sprite 的 `tile` 字段。
- **重建 Scene:** 按收到的顺序调用 `insert_primitive` 和 `finish()`,再交给渲染器的 `draw(&scene)`。

| 形态 | 实现 | 评价 |
|---|---|---|
| **PWA / 浏览器(首选)** | wasm 加 `WgpuRenderer::new_from_canvas`,浏览器支持 WebGPU 就用,否则降级到 WebGL2 | 一份代码覆盖 iPad、Android 和 Linux。键盘和 IME 要自己写隐藏 textarea,可参考 `gpui_web/src/ime_mirror.rs` |
| iOS / iPadOS 原生 | `gpui_apple` 的 Metal 渲染器加 UIKit 壳 | 输入法和手势体验最好 |
| Android / Linux 原生 | `WgpuRenderer::new_from_surface` | 可以做,优先级排在 PWA 之后 |
| 过渡 | 用 Capacitor 或 WKWebView 包装 PWA | 改动最小,还能获得推送通知 |

## 6. 进阶:虚拟窗口(C2,仍然不改 GPUI)

`MirrorPlatform::open_window` 可以返回一个完全虚拟的 `RemoteWindow`:没有原生窗口,尺寸和缩放比例直接用客户端上报的值,自带一个只记账、不上传 GPU 的 CPU atlas,自己控制出帧节奏,永远不会处于不可见状态。

- **效果:** 宿主桌面保持桌面布局,手机上另开一个手机布局的窗口,两者共享同一个 `App`(Project、buffer、agent 线程),手机上的文字是 3x 清晰度。这才是真正意义上的结对编程。
- **难点:**
  1. `WindowParams` 里没有字段能标记"这个窗口要虚拟",只能在调用 `cx.open_window` 前设置一个线程局部变量,让包装层据此分流。都在同一线程上同步执行,所以可靠。
  2. workspace 要允许同一个 Project 同时在两个窗口里打开,需要绕开 `open_paths` 复用已有窗口的逻辑。
  3. 同一个 `AcpThread` 能否同时在两个窗口的会话视图中显示,需要实际验证。
- **建议:** 先做 C1(镜像),但协议从一开始就按"一个窗口一路流"设计,升级到 C2 时客户端不用改。

## 7. 坑

1. **强行在不可见状态下出帧有风险(C1)。** 窗口被遮挡或锁屏时,原生 Metal 层的 `nextDrawable` 可能阻塞或拿不到缓冲。不可见期间应跳过 `inner.draw`、只编码 Scene,并阻止 App Nap。必须先在真机上验证;C2 天然没有这个问题。
2. **Path 不能逐字段复制。** `Path` 有私有字段(`start`、`current`、`contour_count`),客户端要用 `Path::new` 加 `push_triangle` 重新构建。顶点数据是公开的。
3. **`PaintSurface` 只在 macOS 上存在,而且不能序列化**(CVPixelBuffer,用于视频)。跳过或画占位块即可。
4. **atlas 一致性。** 重连、宿主 atlas 淘汰 tile、缩放比例变化时,客户端要整体重新同步。帧消息带 `atlas_epoch`,客户端缺 tile 时请求补发,补齐之前不渲染这一帧。
5. **游离在 Scene 之外的 UI。** macOS 应用菜单不会被镜像;独立的 popup 窗口(`crates/gpui/src/platform/popup.rs`)包装层能拦截到,但要作为另一路流转发;有多个 Zed 窗口时只镜像指定的那一个。
6. **带宽和帧率。** agent 流式输出、光标闪烁会持续出帧,需要限制在 30fps 左右,并在没有变化时不发帧。
7. **输入焦点冲突。** 宿主和手机同时操作时输入会交错。可以接受这种情况,或者在手机连接时让宿主只读。
8. **剪贴板语义。** 远端复制时操作的是宿主剪贴板,需要做双向同步。
9. **私有接口不稳定。** 不改 GPUI 的代价是要追赶 `PlatformWindow` 和 `Scene` 的签名变化,这两处变动频繁,最近就加过 `visibility`、`prepare_frame` 等方法。新方法大多有默认实现,每次同步上游通常只需少量适配。
10. **安全。** 这等同于远程执行代码的能力。必须用 TLS 加强认证(token 加设备绑定),不要直接对公网开端口。

## 8. 里程碑与工作量(粗估)

| 阶段 | 内容 | 工作量 |
|---|---|---|
| M1 | `MirrorPlatform/Window/Atlas`,用 WebSocket 推送全量帧;桌面原生 wgpu 测试客户端,只显示画面 | 1–2 周 |
| M2 | 输入回传(触控、滚动、按键)加 IME 的预编辑和提交;增量帧和压缩 | 1–2 周 |
| M3 | PWA 客户端(WebGPU/WebGL2、软键盘、safe area);认证和 TLS | 2 周 |
| M4 | 宿主手机形态的 UI 调整(agent 面板全屏、隐藏 dock、字号);处理锁屏出帧 | 1 周 |
| M5(可选) | C2 虚拟窗口加第二个 Workspace 窗口 | 2–3 周 |

## 9. 与方案 D 的关系

方案 D 在 Zed 进程内暴露 agent 线程的 API,配原生或网页客户端,和 C 不冲突:

- C 用来完整控制 Zed;
- D 作为轻量通道,只推送 agent 状态加系统推送(比如"agent 需要你授权某个工具调用"),弱网下也能用。

两者可以复用同一套 WebSocket 服务和认证。
