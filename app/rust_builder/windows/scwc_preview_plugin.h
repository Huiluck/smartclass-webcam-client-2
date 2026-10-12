#ifndef FLUTTER_PLUGIN_SCWC_PREVIEW_PLUGIN_H_
#define FLUTTER_PLUGIN_SCWC_PREVIEW_PLUGIN_H_

#include <flutter/method_channel.h>
#include <flutter/plugin_registrar_windows.h>
#include <flutter/texture_registrar.h>

#include <atomic>
#include <cstdint>
#include <memory>

namespace rust_lib {

// The camera preview as a Flutter texture, painted from frames the capture path published.
//
// The interface talks to `scwc_preview` — `start` registers the texture and answers its id, `stop`
// unregisters it, `size` answers the shape of the frame it paints — and the plugin tells the engine
// when a newer frame is waiting. Nothing crosses into Dart, so the picture costs the conversion the
// capture path already pays and no more.
class ScwcPreviewPlugin : public flutter::Plugin {
 public:
  static void RegisterWithRegistrar(flutter::PluginRegistrarWindows* registrar);

  explicit ScwcPreviewPlugin(flutter::PluginRegistrarWindows* registrar);

  virtual ~ScwcPreviewPlugin();

  // Disallow copy and assign.
  ScwcPreviewPlugin(const ScwcPreviewPlugin&) = delete;
  ScwcPreviewPlugin& operator=(const ScwcPreviewPlugin&) = delete;

  // Called when a method is called on this plugin's channel from Dart.
  void HandleMethodCall(
      const flutter::MethodCall<flutter::EncodableValue>& method_call,
      std::unique_ptr<flutter::MethodResult<flutter::EncodableValue>> result);

 private:
  // Registers a texture and starts the capture path publishing into it, answering its id, or
  // `kNoTexture` when the engine refused the texture.
  int64_t StartTexture();

  // Unregisters the texture and stops the capture path.
  void StopTexture();

  // The shape of the frame the texture paints, or null while it has painted none.
  flutter::EncodableValue FrameSize();

  // Fills in the buffer the engine reads the newest frame through, or answers null while nothing
  // has been captured.
  const FlutterDesktopPixelBuffer* CopyPixelBuffer();

  // The capture path's notice that a newer frame is waiting, called from the capture thread.
  static void FrameReady();

  // Frees the frame the engine has finished painting.
  static void ReleasePaintedFrame(void* release_context);

  // The texture registry inside the engine's registrar.
  flutter::TextureRegistrar* textures_;

  // The registered texture, or null while none is.
  std::unique_ptr<flutter::TextureVariant> texture_;
  // The registered texture's id, or `kNoTexture` while none is.
  std::atomic<int64_t> texture_id_;

  // What the engine reads, describing the frame it paints.
  FlutterDesktopPixelBuffer buffer_;
  // The frame the engine paints, or null while it paints none.
  const uint8_t* painted_;

  // The shape of the frame the texture painted last. Written by the engine's copy on the render
  // thread, read by `size` on the platform thread, so it is asked for atomically rather than through
  // a frame the plugin would have to take — and taking one would be a live preview frame thrown away
  // for a number.
  std::atomic<uint32_t> frame_width_;
  std::atomic<uint32_t> frame_height_;
};

}  // namespace rust_lib

#endif  // FLUTTER_PLUGIN_SCWC_PREVIEW_PLUGIN_H_
