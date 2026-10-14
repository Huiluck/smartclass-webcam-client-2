# 预览卡顿分析：定位、修复与验证

日期：2026-10-14 · 设备：vivo V2405A（天玑 9300，1260×2800 @ 560dpi，传感器 90°）· 流参数：1280×720 @ 60fps

## 预览管线（Android）

```
Camera YUV_420_888 (ImageReader, 1280×720, MAX_IMAGES=2)
  → 采集线程 pump(): acquire → 丢旧帧 → 转换 → publish
  → core preview: permit 节流 → BGRA→RGBA 再转换(fit+缩放) → Vec<u8>
  → FRB StreamSink：整帧 Uint8List 过桥
  → Dart PreviewFeed: decodeImageFromPixels → notifyListeners → RawImage
  → _consumed() 回执 → permit 归还（下一帧才允许生产）
```

其他平台（macOS/iOS/Linux/Windows）走零拷贝纹理路径（`scwc_preview` 插件 + core 的
`publish_rgba_frame` 槽）。**Android 不在 `PreviewTexture.available` 里，是唯一整帧过桥 +
Dart 逐帧解码的平台** —— 这是架构层面的固有负荷。

## 根因（按证据排序）

### R1 双转换 + 旋转后 fit 失效（已修，结构性缺陷）

`convert_and_publish` 先对**未旋转**的源做 fit（1280×720 → 盒 1176×1645 得 1176×662），
旋转 90° 得到 662×1176，再交给 core `publish_bgra`——后者对旋转后的帧**再 fit 一次**：
662×1176 → **926×1645（像素 +96% 的上采样）**。于是每帧：

1. `to_bgra`：YUV→BGRA，778K 像素
2. core `scale_to`：BGRA→RGBA **上采样**，1.52M 像素，每像素一次整数除法，新分配 6MB Vec
3. 该帧被**重采样两次**（先旋转缩放、再上采样），画质也受损

设备实测（bench，60 次/场景，release）：

| 阶段 | 旧路径 | 新路径 |
|---|---|---|
| YUV→BGRA @662×1176 | 3.52 ms | — |
| core scale_to→926×1645 | 5.34 ms | — |
| 平面拷贝（3×to_vec/帧） | 0.60 ms | 0（借用） |
| **融合 YUV→RGBA @925×1645 单遍** | — | **6.19 ms** |
| 每帧过桥字节 | 6.1 MB | 6.0 MB |

单遍 = **-30% CPU**、少一次 6MB 分配、少一次重采样。同几何下 oracle 验证融合输出与旧
`to_bgra` 输出**逐字节一致**（仅通道序 R/G/B）。

### R2 单 permit 全回路节流（结构性，未修）

permit 只在 Dart 侧「解码完成 → notify → `_consumed()` 过桥回执」后才归还。相机侧
**必须等整个 Dart 回路**才能生产下一帧：帧率 = 1/(FRB + decode + paint + FRB ack)。
UI 线程任何抖动（GC、`decodeImageFromPixels` 的 6MB ui.Image、逐帧纹理上传）直接停产相机
侧 → 观感卡顿。这是预览卡顿最可能的剩余瓶颈，但**本次未取到设备实测 fps/permit 数据**
（见「回路状态」）。

### R3 每帧堆抖动（部分缓解）

旧路径每帧：3 个平面 Vec + core 6MB Vec + FRB 拷贝 + Dart 6MB ui.Image ≈ 18MB 垃圾/帧
@60fps ≈ 1.1GB/s → GC 暂停表现为卡顿。融合后省掉平面 Vec 与一次 6MB（core 侧仍需一次
`to_vec`，sink API 收所有权）。彻底解法见 R4。

### R4 Android 无纹理路径（架构建议，未实现）

其余四平台零拷贝纹理预览；Android 缺 `scwc_preview` 插件（SurfaceTexture→Flutter Texture）。
补上后：无过桥、无 Dart 解码、无 permit Dart 回路，相机率直显。工作量约 1-2 天，是唯一
能在不降画质前提下把预览成本降到 ~0 的方案。

## 已实施的修改（工作区，未提交）

| 文件 | 改动 |
|---|---|
| `crates/core/src/preview.rs` | 新增 `publish_rgba()`（零转换直通：permit/路由同 BGRA 路径）+ 抽出 `publish_converted_frame`；新增回归测试：旋转先行 fit 不变量、publish_rgba 直通语义 |
| `crates/capture/src/android/preview.rs` | 修复上一轮重构丢失的 `Plane` 本体（android 模块此前**编译不过**）；`to_bgra`→`to_rgba` 融合单遍（旋转先行 fit + RGBA 直写）；平面零拷贝借用（图像存活期内直接读）；复用 rgba 缓冲；删除全部 probe 埋点 |
| `crates/capture/src/android/mod.rs` | 删除 probe 调用（保留 set_suspended 特性） |
| `crates/capture/Cargo.toml` | 删除 android 专用 libc（仅 probe 用过） |
| `crates/capture/src/android/image_reader.rs` | 还原（删 AImage_getTimestamp） |
| `app/lib/src/preview_feed.dart` | 删除 a4f2 调试块与 dart:io 导入（与 HEAD 一致） |
| `crates/capture/src/android/probe.rs` | 删除（临时探针，本就标注用完即删） |

画质影响：修复改变了**重采样次数**（两次→一次最近邻），同几何像素级一致；输出尺寸 926×1645
→ 925×1645（-0.1%）。预览是给人眼看的，无感。

## 验证状态

- ✅ **算法正确**：设备 + host oracle：融合输出 == 旧算法同几何输出（仅通道序）→ IDENTICAL
- ✅ **性能**：真机 ARM：转换 8.86→6.19 ms/帧（-30%）；总路径含平面拷贝 9.46→6.19（-35%）
- ✅ **语法**：rustc 解析级检查通过（仅剩依赖解析类预期报错）
- ⏳ **编译/单测**：本机 Docker 级内核资源耗尽（见下），`cargo test -p scwc-core` 待重启后跑
- ⏳ **端到端 fps**：需重建带 probe 的 APK（probe 已按规程删除）或直接用 DevTools 帧率

## 回路状态（重要）

上一轮的 a4f2 延迟探针（Rust probe.rs + Dart 写文件）已按诊断规程 Phase 6 清除；设备上
的 a4f2.apk 已被清理。**本机自 2026-10-13 20:00 起子进程创建系统性故障**：任何 Rust/Dart
二进制 spawn 子进程报 `OS Error 5` / `All pipe instances are busy (231)`（bash 自身 spawn 正
常），`flutter build apk`、`cargo test`（build script）、`dart analyze` 全部受阻；
PowerShell 工具管道亦失效。**重启机器后**：`cargo test -p scwc-core && cargo check
--target aarch64-linux-android -p scwc-capture`，再 `flutter build apk` 联调。

Bench 工具（可复跑，零依赖）：`target/preview-bench/`（host 直接 `cargo build --release`；
设备 `cargo build --release --target aarch64-linux-android` + adb push）。

## 后续建议（按性价比）

1. **重启后合入本次修改并回归**（core 单测 + android check + 真机 60fps 预览）。
2. 若仍有卡顿：DevTools performance 看 UI 线程，重点验证 R2（permit 回路）——可试
   `_consumed()` 前移到解码之前，或双 permit 对比。
3. 中期：R4 纹理路径（根治，所有成本归零）。
4. 可选：预览盒上限（当前 = UI 面积 × DPR 3.5 ≈ 1.9M 像素）。会降高 DPI 屏清晰度，
   与「不影响画质」目标冲突，不推荐默认开。
