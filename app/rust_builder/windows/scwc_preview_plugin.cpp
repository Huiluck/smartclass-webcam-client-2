// The camera preview as a Flutter texture, painted from frames the capture path published.
//
// The interface talks to `scwc_preview`, and the plugin tells the engine when a newer frame is
// waiting: the frame the capture path converted and scaled is handed straight to the engine, so a
// preview costs neither a bridge hop nor a decode in Dart.

#include "include/rust_lib/scwc_preview_plugin.h"

// This must be included before many other Windows headers.
#include <windows.h>

#include <flutter/method_channel.h>
#include <flutter/plugin_registrar_windows.h>
#include <flutter/standard_method_codec.h>

#include <atomic>
#include <cstdint>
#include <memory>
#include <string>

#include "scwc_preview_plugin.h"

void ScwcPreviewPluginRegisterWithRegistrar(
    FlutterDesktopPluginRegistrarRef registrar) {
  rust_lib::ScwcPreviewPlugin::RegisterWithRegistrar(
      flutter::PluginRegistrarManager::GetInstance()
          ->GetRegistrar<flutter::PluginRegistrarWindows>(registrar));
}

// The texture path's C ABI, exported by the Rust library this plugin is linked with
// (`app/rust_lib/src/lib.rs`). The frames it answers with are the row-major RGBA the engine wants,
// four bytes per pixel, and each one is the plugin's until it hands the pixels back.
//
// `scwc_preview_register_rgba_callback` takes either a callback — called from the capture thread
// once per frame it publishes, so it must return promptly — or null, which stops the path.
extern "C" {
void scwc_preview_register_rgba_callback(void (*callback)(void));
const uint8_t* scwc_preview_latest_rgba(uint32_t* width, uint32_t* height);
void scwc_preview_release_rgba(const uint8_t* pixels);
}

namespace rust_lib {

namespace {

// The channel the interface starts and stops the texture through.
constexpr char kChannelName[] = "scwc_preview";

// No texture is registered; a registrar's ids are its own, so zero cannot be the "none" value.
constexpr int64_t kNoTexture = -1;

// The plugin the capture path notifies, cleared while the plugin is being torn down.
//
// One registrar means one plugin. The notice arrives on the capture thread, which must not walk into
// a plugin the platform thread may be tearing down: the pointer is cleared before anything is
// unregistered, and a notice that arrives anyway finds nothing to do.
std::atomic<ScwcPreviewPlugin*> g_plugin{nullptr};

}  // namespace

// static
void ScwcPreviewPlugin::RegisterWithRegistrar(
    flutter::PluginRegistrarWindows* registrar) {
  auto channel = std::make_unique<flutter::MethodChannel<flutter::EncodableValue>>(
      registrar->messenger(), kChannelName,
      &flutter::StandardMethodCodec::GetInstance());

  auto plugin = std::make_unique<ScwcPreviewPlugin>(registrar);
  ScwcPreviewPlugin* plugin_pointer = plugin.get();
  channel->SetMethodCallHandler([plugin_pointer](const auto& call, auto result) {
    plugin_pointer->HandleMethodCall(call, std::move(result));
  });

  registrar->AddPlugin(std::move(plugin));
}

ScwcPreviewPlugin::ScwcPreviewPlugin(flutter::PluginRegistrarWindows* registrar)
    : textures_(registrar->texture_registrar()),
      texture_id_(kNoTexture),
      buffer_{},
      painted_(nullptr),
      frame_width_(0),
      frame_height_(0) {}

ScwcPreviewPlugin::~ScwcPreviewPlugin() { StopTexture(); }

void ScwcPreviewPlugin::HandleMethodCall(
    const flutter::MethodCall<flutter::EncodableValue>& method_call,
    std::unique_ptr<flutter::MethodResult<flutter::EncodableValue>> result) {
  const std::string& method = method_call.method_name();
  if (method == "start") {
    const int64_t texture_id = StartTexture();
    if (texture_id == kNoTexture) {
      result->Error("no-texture", "the engine has no room for another texture");
    } else {
      result->Success(flutter::EncodableValue(texture_id));
    }
    return;
  }
  if (method == "stop") {
    StopTexture();
    result->Success();
    return;
  }
  if (method == "size") {
    result->Success(FrameSize());
    return;
  }
  result->NotImplemented();
}

int64_t ScwcPreviewPlugin::StartTexture() {
  if (texture_) {
    // An interface that starts twice means the texture it already has.
    return texture_id_.load();
  }
  auto texture = std::make_unique<flutter::TextureVariant>(
      // The engine offers the surface size it means to paint at; the frame answers with its own,
      // which is the size the capture path scaled it to.
      flutter::PixelBufferTexture(
          [this](size_t, size_t) { return CopyPixelBuffer(); }));
  const int64_t texture_id = textures_->RegisterTexture(texture.get());
  if (texture_id < 0) {
    return kNoTexture;
  }
  texture_ = std::move(texture);
  texture_id_.store(texture_id);
  // The notice arrives on the capture thread, and it needs the plugin before a frame can be
  // published into it.
  g_plugin.store(this);
  // A texture is a consumer: the capture path scales its next frames to the size the interface asked
  // for and offers them here.
  scwc_preview_register_rgba_callback(FrameReady);
  return texture_id;
}

void ScwcPreviewPlugin::StopTexture() {
  // Nothing is published into a plugin that is going away.
  g_plugin.store(nullptr);
  scwc_preview_register_rgba_callback(nullptr);
  if (!texture_) {
    return;
  }
  // Out of the engine's hands first, so nothing paints from a frame that is about to go. The texture
  // goes with the unregistration, which the engine completes on its own thread.
  std::shared_ptr<flutter::TextureVariant> texture = std::move(texture_);
  textures_->UnregisterTexture(texture_id_.load(), [texture]() {});
  texture_id_.store(kNoTexture);
  if (painted_ != nullptr) {
    scwc_preview_release_rgba(painted_);
    painted_ = nullptr;
  }
}

flutter::EncodableValue ScwcPreviewPlugin::FrameSize() {
  if (!texture_) {
    return flutter::EncodableValue();
  }
  const uint32_t width = frame_width_.load();
  const uint32_t height = frame_height_.load();
  if (width == 0 || height == 0) {
    return flutter::EncodableValue();
  }
  return flutter::EncodableValue(flutter::EncodableList{
      flutter::EncodableValue(static_cast<int64_t>(width)),
      flutter::EncodableValue(static_cast<int64_t>(height))});
}

const FlutterDesktopPixelBuffer* ScwcPreviewPlugin::CopyPixelBuffer() {
  uint32_t frame_width = 0;
  uint32_t frame_height = 0;
  const uint8_t* pixels = scwc_preview_latest_rgba(&frame_width, &frame_height);
  if (pixels == nullptr) {
    // Nothing has been captured yet, or the capture path is not publishing for a texture yet. The
    // engine asks again when the path says a frame is waiting.
    return nullptr;
  }
  painted_ = pixels;
  buffer_.buffer = pixels;
  buffer_.width = frame_width;
  buffer_.height = frame_height;
  buffer_.release_callback = ReleasePaintedFrame;
  buffer_.release_context = this;
  frame_width_.store(frame_width);
  frame_height_.store(frame_height);
  return &buffer_;
}

// static
void ScwcPreviewPlugin::FrameReady() {
  ScwcPreviewPlugin* plugin = g_plugin.load();
  if (plugin == nullptr) {
    return;
  }
  // The registrar's members are all safe to call from any thread.
  plugin->textures_->MarkTextureFrameAvailable(plugin->texture_id_.load());
}

// static
void ScwcPreviewPlugin::ReleasePaintedFrame(void* release_context) {
  ScwcPreviewPlugin* plugin = static_cast<ScwcPreviewPlugin*>(release_context);
  plugin->painted_ = nullptr;
  scwc_preview_release_rgba(plugin->buffer_.buffer);
}

}  // namespace rust_lib
