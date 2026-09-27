# Navop macOS 内存占用排查记录

## 1. 问题概述

Navop 0.15.2 在 macOS 上运行一段时间后，Activity Monitor 显示内存约 1.8 GB。初始实测 footprint 为约 1.87 GB，峰值约 2.15 GB。

后续执行 `heap --forkCorpse` 等深度诊断后，目标进程 footprint 被采样操作扰动到约 2.07 GB，主要增加在 malloc allocator 的保留区。因此后续优化应以应用冷启动和固定操作流程重新建立基线，不能直接把 2.07 GB 当作自然使用状态。

本次排查结论：

- 独立弹窗的关闭 API 没有用错，`window.remove_window()` 是正确用法。
- App 层会在窗口更新流程中移除已标记关闭的窗口。
- 主要占用来自 GPUI/Metal 渲染资源，而不是传统 Rust 对象泄漏。
- 当前最明确的问题是全局 `InstanceBufferPool` 无上限缓存 Metal buffer。
- 进程同时保留了 8 个 `GPUIView`、8 个 `CAMetalLayer`，但只有 1 个窗口处于 onscreen 状态，需要继续验证这些 renderer 是否对应仍存活的独立弹窗。

## 2. 现场采样

目标进程：

```text
PID:       75089
Executable: /Applications/Navop.app/Contents/MacOS/navop
Version:   0.15.2
Platform:  macOS ARM64
Launch:    2026-09-01 13:18:30
```

### 2.1 footprint 分布

初始、较有代表性的 footprint 约 1.87 GB：

| 分类 | 大小 | 说明 |
| --- | ---: | --- |
| `IOAccelerator (graphics)` | 1007 MB | Metal/GPU 资源，最大项 |
| `MALLOC_LARGE` | 477 MB | 大块普通分配及 allocator 保留区 |
| `IOSurface` | 237 MB | CAMetalLayer drawable |
| `MALLOC_SMALL` | 110 MB | 普通小对象和分配碎片 |
| `owned unmapped (graphics)` | 24 MB | GPU 相关未映射物理资源 |
| 线程栈实际驻留 | 约 1 MB | 不是主要来源 |

执行深度 heap/corpse 分析后，`MALLOC_LARGE` 一度上升到约 659 MB，footprint 稳定在约 2.067 GB。GPU 和 IOSurface 分类基本不变，说明这部分额外增长主要是诊断造成的 allocator 扰动，不应作为应用自身泄漏证据。

5 秒连续采样没有看到持续线性增长。

### 2.2 Metal 窗口和 drawable

`heap`/`vmmap` 观察到：

- `GPUIView`: 8 个。
- `CAMetalLayer`: 8 个。
- `CAMetalLayer Display Drawable`: 24 个。
- GPUI 每个 layer 设置 `maximum_drawable_count = 3`，所以 8 个 layer 正好对应 24 个 drawable。
- CoreGraphics 窗口列表中只有 1 个 onscreen 窗口，说明其余 renderer 可能是隐藏窗口、非 onscreen 窗口，或者仍未完成释放。

drawable 示例：

- 3 个约 12.1 MB 的 `2200x1400` surface。
- 3 个约 18.4 MB 的 `2742x1718` surface。
- 多个约 8.0 MB 的 `1400x1440` surface。

这些 surface 合计约 237 MB，与 `IOSurface` footprint 分类一致。

### 2.3 16 MB buffer 证据

`vmmap`/`heap` 发现：

- 28 个精确的 16 MB raw allocation，约 448 MB。
- 15 个 `AGXG16GFamilyBuffer` 和 13 个 `AGXBuffer`，合计 28 个 Metal buffer 对象。
- 代码中的 GPUI `InstanceBufferPool` 初始 buffer size 为 2 MB，渲染失败时按 2 倍增长，可能增长到 16 MB。

由于目标进程是 hardened/ad-hoc bundle，系统工具无法读取完整的 malloc allocation backtrace，因此“28 个 16 MB 分配就是 28 个 instance buffer”的结论属于强关联证据，不是符号级 100% 证明。但数量、大小和对象类型完全吻合，应优先按此方向修复和验证。

### 2.4 传统泄漏检查

`leaks` 结果约为：

```text
356 nodes
17.76 KB leaked
```

这不支持“数百 MB 已不可达泄漏”的判断。当前问题更像是资源仍然可达，但被全局缓存或仍存活的 renderer 持有。

## 3. 关闭流程是否正确

### 3.1 App 层

GPUI 的 `Window::remove_window()` 只是把当前窗口标记为 removed：

```text
gpui-ce/crates/gpui/src/window.rs:2111-2114
```

真正移除发生在 `App::update_window_id` 的收尾逻辑：

```text
gpui-ce/crates/gpui/src/app.rs:1881-1924
```

当 `window.removed` 为 true 时，会移除：

- `cx.window_handles`
- `cx.windows`
- 窗口关联的 entity invalidator
- 已关闭窗口观察者

因此业务层调用 `window.remove_window()` 的姿势是正确的。

### 3.2 独立弹窗

独立弹窗统一通过：

```text
navop/crates/core/src/popup_window.rs:148-256
```

创建为普通 GPUI window，并注册窗口关闭处理。

复用弹窗（`open_reusable_popup_window`，登记了复用键）的关闭路径**不再是** `remove_window()`：

- **原生窗口**：`orderOut` 隐藏后登记复用。原因是在 macOS 上销毁原生窗口会触发 AppKit
  Touch Bar 观察者向已 dealloc 的对象注销，抛出的 ObjC 异常无人接住 ⇒ 闪退（issue #262）。
- **业务会话**：关闭时立即结束 —— 卸载业务 view 及它持有的数据、连接与任务句柄，清掉焦点与通知。

```text
navop/crates/core/src/window_close.rs      # close_window_for_reuse(window, cx)
navop/crates/core/src/popup_window.rs      # end_reusable_popup_session / PopupWindowContent::end_session
navop/crates/core/src/popup_lifecycle.rs   # live_windows / live_sessions 计数与日志
```

关键约束：**复用窗口不等于复用业务状态**。只隐藏不卸载，业务 view 会被仍然存活的内容树一直强引用着 ——
用户不再打开那类窗口时就是纯泄漏（注册表里只存 `WeakEntity`，管不到它）。
没有登记复用键的窗口仍然走 `remove_window()`；不要用 `minimize_window()` 代替关闭。

### 3.3 macOS 平台层

macOS `MacWindow` 被 Rust 层释放时：

```text
gpui-ce/crates/gpui_macos/src/window.rs:1181-1203
```

当前流程会调用 `renderer.destroy()`，随后异步执行 native window close 和 autorelease。

问题是：

- `MetalRenderer::destroy()` 当前是空实现：
  `gpui-ce/crates/gpui_macos/src/metal_renderer.rs:582-584`
- 全局共享的 `InstanceBufferPool` 不属于单个 renderer，renderer 关闭后仍会保留 buffer。
- `setReleasedWhenClosed:NO` 要求后续 native window 生命周期处理必须可靠完成；这条链路需要通过关闭后 layer 数量下降来验证。

## 4. 代码原因

### 4.1 全局共享的 InstanceBufferPool 无上限

macOS 平台状态创建一个共享 renderer context：

```text
gpui-ce/crates/gpui_macos/src/platform.rs:171-227
gpui-ce/crates/gpui_macos/src/platform.rs:654-682
```

每个 MacWindow 都 clone 同一个 `renderer_context`，类型为：

```rust
Arc<Mutex<InstanceBufferPool>>
```

池的行为：

```text
gpui-ce/crates/gpui_macos/src/metal_renderer.rs:74-127
```

- 默认大小 2 MB。
- 不够用时创建当前大小的 Metal buffer。
- 渲染完成后放回 `buffers`。
- `buffers` 没有数量上限或总字节上限。
- 窗口关闭不会清理这个全局池。

因此只要历史上有多个窗口同时渲染，或者有多个较复杂 scene 同时提交，池就可能保留大量 16 MB buffer。当前约 28 个 16 MB allocation 与此行为高度吻合。

### 4.2 每个 renderer 预分配多张离屏纹理

窗口尺寸变化时：

```text
gpui-ce/crates/gpui_macos/src/metal_renderer.rs:496-573
```

会创建：

- path intermediate texture
- scene color texture
- 两张 group texture
- 两张 half-resolution blur texture
- 额外的 MSAA texture

当前实现即使 scene 没有 blur/filter，也会创建 scene/group/blur 相关纹理。这会放大多窗口场景下的 GPU 占用。它是明确的优化点，但目前没有证据证明它单独造成了关闭后的长期泄漏。

### 4.3 8 个 renderer 的生命周期需要确认

采样显示 8 个 GPUIView/CAMetalLayer，而系统只有 1 个 onscreen 窗口。可能原因：

- 仍有多个独立弹窗实际存活但不可见。
- native window 已关闭，但 Objective-C retain/autorelease 尚未完成。
- renderer 尚存于 GPUI/平台层引用中。
- 某些窗口在使用过程中被隐藏，而不是走 remove 流程。复用弹窗是**有意如此**（见 3.2）：
  它属于「受控的固定保留」，上限是复用键数量；用 `popup_lifecycle` 的 `live_windows` / `live_sessions`
  计数把它跟「持续增长」区分开。

需要加入窗口创建、关闭、`MacWindow::drop` 和 renderer 数量日志，才能把这 8 个 renderer 映射到具体窗口。

## 5. 推荐修改方案

建议按风险从低到高分阶段修改。

### 阶段一：限制 InstanceBufferPool 缓存

目标：先把约 448 MB 的 16 MB buffer 缓存压到可控范围。

在 `metal_renderer.rs` 增加具名上限，例如：

```rust
const MAX_CACHED_INSTANCE_BUFFERS: usize = 8;
const MAX_CACHED_INSTANCE_BUFFER_BYTES: usize = 128 * 1024 * 1024;
```

调整 `InstanceBufferPool::release`：

- size 不匹配时直接丢弃。
- 已缓存数量达到上限时直接丢弃。
- 已缓存字节数达到上限时直接丢弃。
- 只缓存已完成 command buffer 的 buffer。

参考实现：

```rust
const MAX_CACHED_INSTANCE_BUFFERS: usize = 8;
const MAX_CACHED_INSTANCE_BUFFER_BYTES: usize = 128 * 1024 * 1024;

pub(crate) fn release(&mut self, buffer: InstanceBuffer) {
    if buffer.size != self.buffer_size {
        return;
    }

    let cached_bytes = self.buffers.len().saturating_mul(buffer.size);
    let can_cache = self.buffers.len() < MAX_CACHED_INSTANCE_BUFFERS
        && cached_bytes.saturating_add(buffer.size) <= MAX_CACHED_INSTANCE_BUFFER_BYTES;

    if can_cache {
        self.buffers.push(buffer.metal_buffer);
    }
}
```

建议初始上限为 8 个或 128 MB，不建议一开始设置为 3 个，因为多窗口并发渲染可能造成频繁申请和释放。后续依据帧率和内存数据再调小。

### 阶段二：让 renderer destroy 清理高占用资源

将：

```rust
pub fn destroy(&self) {}
```

改为可变清理方法，并在 `MacWindow::drop` 中调用：

- `path_intermediate_texture = None`
- `path_intermediate_msaa_texture = None`
- `scene_color_texture = None`
- `blur_ping_texture = None`
- `blur_pong_texture = None`
- `group_textures.clear()`
- 必要时释放或断开 `CAMetalLayer`

参考结构：

```rust
pub fn destroy(&mut self) {
    self.path_intermediate_texture = None;
    self.path_intermediate_msaa_texture = None;
    self.scene_color_texture = None;
    self.blur_ping_texture = None;
    self.blur_pong_texture = None;
    self.group_textures.clear();
}
```

该方法应设计为幂等。不要在 command buffer 仍可能使用资源时绕过 Metal 引用计数；只清理 Rust 持有的引用，实际资源由 Metal 在 GPU 完成后回收。

注意：这一阶段不能清理全局 `InstanceBufferPool`，因为它被所有窗口共享。buffer pool 必须通过阶段一的容量限制，或者单独增加显式 trim API。

### 阶段二补充：验证 native window 是否及时析构

先用日志确认以下顺序是否完整发生：

1. 业务层调用 `window.remove_window()`。
2. `App::update_window_id` 移除窗口。
3. `MacWindow::drop` 执行。
4. `dealloc_view` 和 `dealloc_window` 最终执行。

只有前三步发生、第四步长期不发生时，再调整 Objective-C 释放策略。可选方向：

- 在异步 native close 闭包中创建局部 `NSAutoreleasePool` 并在 close 后 drain。
- 在关闭前停止 display link。
- 将 native view 从 superview 移除，并断开它持有的 CAMetalLayer。
- 检查 `setReleasedWhenClosed:NO` 对应的 retain 是否被可靠平衡。

示意代码：

```rust
this.foreground_executor
    .spawn(async move {
        unsafe {
            let pool = NSAutoreleasePool::new(nil);

            if let Some(parent) = sheet_parent {
                let _: () = msg_send![parent, endSheet: window];
            }

            window.close();
            window.autorelease();
            pool.drain();
        }
    })
    .detach();
```

这部分改动的生命周期风险高于 buffer pool 限制，必须在确认 native dealloc 没有发生后再做，不能仅凭 `renderer.destroy()` 为空就直接替换为手动 `release`。

### 阶段三：离屏纹理惰性创建

根据 scene 是否包含 blur/filter 决定是否创建：

- scene color texture
- group textures
- blur ping/pong textures

没有 filter 时不创建这些纹理；从有 filter 切换到无 filter 时可以清理或延迟清理。path rasterization 所需纹理要根据实际调用路径单独保留，不能直接全部删除。

### 阶段四：加入资源生命周期诊断

建议增加低频日志或 debug 计数：

- renderer created/dropped 数量
- `MacWindow` created/dropped 数量
- `InstanceBufferPool` 当前 buffer 数量和字节数
- 每个 buffer 的 size
- `CAMetalLayer` 创建和销毁数量
- 弹窗关闭后的窗口 ID

不要在每一帧打印日志，避免日志本身影响性能和内存。

## 6. 构建注意事项

Navop 当前使用 GPUI CE 的 git 依赖：

```text
navop/Cargo.toml:61-64
rev = 9086e0b273bddc083fb030a8aadfc27767eda88e
```

实际编译使用的是 Cargo git checkout 中的对应 revision，不是自动使用旁边的 `gpui-ce` 工作区目录。修改 GPUI 后需要：

1. 临时改成 path dependency，或
2. 提交 GPUI 修改并更新 Navop 的 git revision。

不要直接修改 `.cargo/git/checkouts` 下的临时源码作为最终方案。

## 7. 验证标准

### 7.1 关闭流程

重复执行以下操作：

1. 启动 Navop。
2. 连续打开多个独立弹窗，例如 SSH、数据库或新建连接窗口。
3. 分别通过取消、标题栏关闭按钮和系统关闭按钮关闭。
4. 等待 2-5 秒。
5. 再执行内存采样。

预期：

- GPUIView/CAMetalLayer 从 8 回落到接近 1。
- drawable 从 24 回落到接近 3。
- 关闭弹窗后 `InstanceBufferPool` 不超过设定上限。
- footprint 明显下降，而不是只在重启后下降。

### 7.2 建议命令

```bash
footprint --pid <PID>
vmmap -summary <PID>
heap -sH <PID>
leaks --noContent <PID>
```

重点比较：

- `IOAccelerator (graphics)`
- `IOSurface`
- `MALLOC_LARGE`
- `GPUIView` 数量
- `CAMetalLayer` 数量
- 16 MB allocation 数量

### 7.3 回归风险

- buffer 上限过小可能造成 Metal buffer 频繁申请，表现为 CPU 占用上升或帧率下降。
- renderer 清理需要兼容 command buffer 异步完成。
- blur 惰性创建可能影响首帧，需要验证窗口 resize、透明标题栏和 blur UI。
- 必须同时验证主窗口、独立弹窗、最小化窗口和多窗口并发渲染。

## 8. 最终判断

用户关闭独立弹窗的操作不是根因。当前优先级如下：

1. 先限制全局 `InstanceBufferPool`，这是最明确且收益最大的修改。
2. 再验证关闭后 8 个 renderer 是否下降到 1 个。
3. 若 renderer 数量不下降，继续修复 macOS native window/renderer 生命周期。
4. 最后将 blur/filter 离屏纹理改为惰性创建，降低正常多窗口场景的 GPU 基线。

## 9. 复用弹窗的现状与判据

复用弹窗（登记了复用键的那些）在关闭时**不销毁原生窗口**，而是 `orderOut` 隐藏并结束业务会话（见 3.2）。
这不是「泄漏」，但必须有判据，否则无法把它和真正的增长区分开。`crates/core/src/popup_lifecycle.rs` 提供：

| 计数 | 含义 | 正常表现 |
|---|---|---|
| `live_windows` | 登记在册（隐藏后等待复用）的原生窗口 | 每类弹窗首次打开 +1，之后开关不再增长；上限＝复用键数量 |
| `live_sessions` | 当前仍持有业务 view 的弹窗 | 关闭后回落到 0；随开关次数持续上涨＝旧会话没卸载 |
| `opened_windows` | 累计创建的原生窗口 | 只在复用没命中（退化成每次新建）时随开关次数上涨 |
| `opened_sessions` | 累计打开的业务会话 | 每次打开 +1，属预期 |

日志 target 为 `one_core::popup_lifecycle`（stage 取 `popup_window_registered` / `popup_window_unregistered` /
`popup_session_ended` / `popup_session_reopened`），只记数字，不记标题、路径或业务内容。

采样时的判断顺序：先看 `opened_windows` 是否随开关次数上涨（是 ⇒ 复用没命中，问题不在保留策略）；
再看 `live_sessions` 是否回落（否 ⇒ 会话没卸载）；两者都正常但 footprint 仍涨，才回到第 4、5 节找 GPU/renderer 侧原因。

注意：不要给复用注册表加 LRU 淘汰来「限制保留」——淘汰即销毁，等于把 Touch Bar 崩溃挪到淘汰路径上。
注册表按 `&'static str` 复用键组织，结构上已有上限。

## 10. 原生窗口退役方案：试过、被否，以及原因（2026-09-26）

背景：`fork-0.3.104` 起关窗会真正释放原生窗口，AppKit 的 Touch Bar 观察者可能在被观察视图
析构之后才注销观察 → 未捕获 ObjC 异常 → 进程终止（上游单据 zed-industries/zed#64819）。
「等一段时间再释放」已经被现场否证（v0.18.6 = `fork-0.3.110`、v0.19.1 = `fork-0.3.114`
都带 100 ms 等待，帧序列一致，仍然崩），于是试了另一个方向：
**关闭后不释放原生窗口，只把重资源摘出来**（`MacWindow::drop` 里把 `MacWindowState`
从 `windowState` ivar 上摘掉，原生窗口交给一张进程级退役表 `window_teardown::retire`）。

这份实现在 review 中被否，理由不是风格问题，而是明确的正确性/边界问题：

1. **阻断：先摘状态、再 `close()`，会走空指针 `Arc`。**
   GPUI 在 `GPUIWindow` 上注册了自己的 `close`（`window.rs:487` →
   `close_window`，`window.rs:3255`），它第一步就是 `get_window_state(this)`；
   而 `get_window_state`（`window.rs:2468`）不判空就直接 `Arc::from_raw`。
   所以「`take_window_state(window)` → 之后 `window.close()`」这条链**必然**用空指针构造
   `Arc`（UB），与 Touch Bar 无关，是每次关窗都走——而且不能简单把 `close()` 提前了事：
   `MacWindow::drop` 持有状态锁，`close_window` 会再次锁同一个状态。

2. **退役后的原生对象仍会被发消息。**
   `reset_cursor_rects`（`window.rs:2552`）、`make_backing_layer`（`3279`）、
   `view_did_change_backing_properties`（`3285`）、`set_frame_size`（`3290`）等注册在
   窗口/视图类上的方法都无条件 `get_window_state`。把 delegate 置空解决不了这个，
   需要显式区分 Active / Closing / Retired，并让每个原生入口在退役后安全返回
   （默认值 / 调用 superclass / 忽略事件）。仅 `window.rs` 内 `get_window_state`
   就有 42 处调用点，这才是这件事的真实工作量；在一处补 `if raw.is_null()` 不够。

3. **退役表是无界累积，不是复用池。**
   每真正销毁一个 GPUI 窗口就多留一个原生窗口常驻；`Vec<usize>` 只记录地址，
   本身不做 Objective-C retain，将来清空列表也不会释放窗口。它可以定义为
   「有意的泄漏止血」，但不能当作内存增长问题的解决方案。

4. **「只留空窗口、GPU 已释放」的说法不成立。**
   断开的是 `原生对象 → MacWindowState`；未处理
   `NSWindow → contentView → GPUIView → backing CAMetalLayer`
   （`native_view.setWantsLayer(YES)`，`makeBackingLayer` 返回 renderer 的 layer）。
   而 `gpui_apple::metal_renderer::destroy()` 是空实现（`metal_renderer.rs:444`），
   所以「释放了 Rust renderer 的引用」不等于「原生 layer 被回收」，这部分必须实测，
   不能靠注释断言。

结论：**原生销毁作为独立问题继续修，先不动底层。** 主线仍是
「有界复用（隐藏不销毁）＋ 关闭即结束业务会话」，它已在
`crates/core/src/window_close.rs` / `crates/core/src/popup_window.rs` 落地且有现场数据支撑。
若以后重启退役方案，前置条件是上面 1–3 全部补齐（其中 2 需要一次状态机式的生命周期改造），
而不是扩大 `retire()` 的使用范围。

两条容易误判的边界：

- Navop 目前的隐藏路径（`window_close.rs` 的 `orderOut:`）**不会**进入 `MacWindow::drop`，
  所以即使底层退役方案成立，也不会自动解决隐藏窗口自身的资源保留。
- 根 `Cargo.toml` 的 `[patch.crates-io]` 曾指向 `fork-0.3.115`；本地 zed checkout 编译通过
  **不等于** navop 用上了这份改动，集成必须走 gpui-pre 快照 / 新 tag
  （2026-09-27 已切到 `fork-0.3.116`，见 §10.3）。

实验版代码先落在 zed checkout 的本地分支 `experiment/native-window-retire`
（`440483b8ee` 保留作对照，修好的版本见 §10.1 / §10.2 的两个提交），
现已进入发布分支 `publish/gpui-pre-0.3.116`（见 §10.3）。

### 10.1 重做后的状态（2026-09-26 晚）

review 的 4 条全部按上面 1–3 补齐，重做提交 `827b2105a3`
（`gpui_macos: Retire a native window without detaching its state or holding its GPU resources`，
与 `440483b8ee` 相邻，仍在本地 `experiment/native-window-retire` 分支）：

- **不再分离状态。** `windowState` ivar 全程有效，`renderer` 改为 `Option`，加入 `retired` 标记；
  资源改在**关闭之后**于原地释放（`MacWindowState::retire`），因此 `close_window`
  仍能看到完整状态（空指针 `Arc` 路径消失）。
- **入口安全化。** `makeBackingLayer`（回退 super）、`viewDidChangeBackingProperties`、
  `setFrameSize:`（调 super、不调 drawable 尺寸）、`displayLayer:`、`resetCursorRects`、
  `viewDidChangeEffectiveAppearance` 都按语义安全返回；输入/拖拽/标签页/delegate 入口
  在退役后不可达（离屏、非 key、非 first responder、delegate 已置 nil）。
- **释放被证明。** `retire()` 丢弃 renderer（连带 metal layer、drawable pool、纹理）、
  accessibility adapter 与**全部回调**（回调会扣住 GPUI 实体，不清就等于把上面那份
  「业务会话不卸载」的问题挪到底层）；`window_teardown::release_layer` 用
  `setWantsLayer:NO` 断开视图对 metal layer 的持有，并在 layer 仍在时告警。
- **边界明确为「按使用量有界的保留」。** 每次退役记一条日志，累计达到
  `RETIRED_WINDOWS_WARN_THRESHOLD`（32）时告警一次；不设上限、不做淘汰，因为
  淘汰即释放（会崩）。若导航器发现在长会话里窗口翻页量很大，再考虑原生窗口池。

验证状态：zed 侧 `cargo check / clippy / fmt` 全绿；**本机没有 Touch Bar，
崩溃本身既未能复现也未能证明修好**。字段判据是 `window_teardown` 的退役计数日志
与「a retired window's view still holds a layer」告警。

### 10.2 可跑的验证与 CI（2026-09-26 深夜）

用测试把「退役后原生对象存活、renderer 的 metal layer 已释放」变成可核验的判据，
提交 `8af22191af`（`gpui_macos: Probe what retiring a native window guarantees`，
仍在本地 `experiment/native-window-retire`）：

- 探针必须跑在**进程主线程**上：AppKit 建窗/显示窗口时抛的 Objective-C 异常无法被 Rust
  捕获，libtest 又在自己的测试线程上跑用例（实测直接 SIGABRT）。做法是测试重新执行本测试
  二进制、带上 `GPUI_MACOS_TEARDOWN_PROBE`，由 `#[ctor]` 在 libtest 之前于主线程完成探针并
  退出；测试只读子进程退出码 —— 这样 `abort` 也会表现为失败，而不是挂住不返回。
- 探针实际抓到一个真问题：`setWantsLayer: NO` **不会**立刻释放视图的 layer，AppKit 要等到
  该视图的下一次 display cycle，而退役窗口离屏、永远不会再有 —— 于是 layer（连同它的
  drawable pool 与纹理）继续被扣着。`release_layer` 现在在置 `setWantsLayer: NO` 之后
  再把 layer 摘掉，探针断言的正是这个最终状态。
- 跑法：`cargo test -p gpui_macos --lib window_teardown`（本机 arm64 通过；
  同 crate 全部 8 个测试通过，`clippy --all-targets` 与 `fmt --check` 全绿）。

CI：`.github/workflows/macos-window-teardown.yml`（fork 专用，不要提到上游 PR），
在 `macos-15-intel`（x86_64）与 `macos-latest`（arm64）上跑同一个探针。

**明确说明：这个 workflow 复现不了崩溃本身。** 变量不是架构而是 Touch Bar ——
`_NSTouchBarFinder` 只有插着 Touch Bar 时才装了观察者，GitHub 托管 runner（Intel 与
arm 都一样）没有 Touch Bar。因此 CI 能钉住的是「修复所依赖的不变量」，真正的 abort
回归只能在带 Touch Bar 的机器上做（自托管 runner 或人工真机）。

### 10.3 发布与集成（2026-09-27）

退役方案不再是「只在本地的实验」，已经发成 gpui-pre 快照并被 navop 用上：

- **发布基线必须是 `gpui-pre-release`，不是退役提交所在的（纯净）upstream 基线。**
  navop 的 `remote_desktop_view` 用了 fork-only API（`DynamicTexture` /
  `Window::update_dynamic_texture`），它只在 `gpui-pre-release` 上；
  当前 `upstream/main` 里 0 处出现。两个基线在 `crates/gpui_macos` 上逐字节相同，
  所以换基线只是换基座，cherry-pick 无冲突。
- **发布分支 `publish/gpui-pre-0.3.116`**（zed checkout）：`gpui-pre-release`
  → revert 掉旧 100 ms 宽限期（`0c2f3ae37f`，已被退役取代，现场已否证）
  → cherry-pick 三个退役提交（`440483b8ee` → `827b2105a3` → `8af22191af`）。
  分支上 `cargo test -p gpui_macos --lib` = 8 passed / 0 failed（含退役探针）、
  `cargo fmt -p gpui_macos -- --check` 通过；`cargo clippy` 在
  `crates/gpui/src/window.rs:5154` 报基线自带的 `redundant_clone`（upstream 在新版里已修，
  本次未触碰该 crate），只影响在快照上跑 clippy，不影响 navop 构建。
- **快照 `fork-0.3.116`** 已发到 `feigeCode/gpui-pre`（提交 `3f3fd66`）。相对
  `fork-0.3.115` 的 delta 只有版本号、`crates/gpui_macos/src/window.rs`（229 行）、
  新增 `window_teardown.rs`（348 行）与 `gpui_macos.rs` 里的 `mod` 声明 ——
  快照里的两个文件与 zed 源**逐字节相同**。
- **navop 侧**：`[patch.crates-io]` 的 24 处与 `Cargo.lock` 已切到 `tag = "fork-0.3.116"`
  （由 `script/migrate-to-git-fork.py` 改写），`[workspace.dependencies]` 的
  `gpui-pre = "0.3.99"` 是 caret 范围，无需改动。
- 真机判据不变（§10.2）：带 Touch Bar 的机器上反复开关窗口，看进程是否存活，
  以及 `window_teardown` 的退役计数日志、「a retired window's view still holds a layer」告警。
