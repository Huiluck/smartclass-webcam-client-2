# 项目长期记忆 — smartclass-webcam-client-2

## 预览路径结构（2026-10-14 诊断结论）
- 两条预览路径：stream 路径（FRB 桥 + Dart 逐帧 decodeImageFromPixels，sink 节流 permit）
  与纹理路径（各平台 scwc_preview 插件 + core `publish_rgba_frame` 槽，零拷贝）。
  **Android 只有 stream 路径**（`PreviewTexture.available` 不含 Android）——唯一整帧过桥 +
  Dart 解码的平台，固有高负荷。
- 旋转传感器的 Android 预览：**必须先对"展示形状"fit 再转换**（四分之一旋转先换源边长），
  否则 core 会二次 fit 上采样（实测 662×1176→926×1645，+96% 像素）。
- core `publish_rgba()` = 生产者已转换好的直通入口（permit/路由与 publish_bgra 相同）。
- 设备真机 ARM 基准：预览盒 ≈ UI 面积 × DPR（vivo 560dpi→3.5）；融合单遍 YUV→RGBA
  6.19ms/帧 vs 旧双遍 8.86ms/帧 @1280×720 源、60fps 相机下转换均非瓶颈。

## 环境事实
- 本机 Windows 会话如出现 "All pipe instances are busy"/`OS Error 5` on Rust/Dart 子进程
  spawn → 内核资源耗尽，**重启前不要浪费时间调试**（bash spawn 正常是陷阱）。
- 交叉编译到 android：NDK 28.2 的 clang++.exe 作 linker，必须
  `-C link-arg=--target=aarch64-linux-android21`（NDK bin 里有 clang++.exe）。
- Git Bash 会转换 MSYS 风格路径 → adb 命令加 `MSYS_NO_PATHCONV=1`。
- adb 真机联调：`adb reverse tcp:8080 tcp:8080` + prefs 种子
  (`/data/data/com.example.webcam_client/shared_prefs/FlutterSharedPreferences.xml`，
  包名 com.example.webcam_client) + `pm grant CAMERA` + monkey 启动。

## 工作纪律
- 未提交改动必须明说（git status 为准）；诊断遗留的临时 instrumentation 用完即删。
- target/ 下 bench 工具 `target/preview-bench/` 可复跑（零依赖，host/device 双跑）。
