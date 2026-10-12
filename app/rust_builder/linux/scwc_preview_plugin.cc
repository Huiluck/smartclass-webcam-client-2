// The camera preview as a Flutter texture, painted from frames the capture path published.
//
// The interface talks to `scwc_preview`, and the plugin tells the engine when a newer frame is
// waiting: the frame the capture path converted and scaled is handed straight to the engine, so a
// preview costs neither a bridge hop nor a decode in Dart.

#include "include/rust_lib/scwc_preview_plugin.h"

#include <flutter_linux/flutter_linux.h>

#include <atomic>
#include <cstdint>

// The texture path's C ABI, exported by the Rust library this plugin is built with
// (`app/rust_lib/src/lib.rs`). The frames it answers with are the row-major RGBA the engine wants,
// four bytes per pixel, and each one is the plugin's until it hands the pixels back.
//
// `scwc_preview_register_rgba_callback` takes either a callback — called from the capture thread
// once per frame it publishes, so it must return promptly — or NULL, which stops the path.
extern "C" {
void scwc_preview_register_rgba_callback(void (*callback)(void));
const uint8_t* scwc_preview_latest_rgba(uint32_t* width, uint32_t* height);
void scwc_preview_release_rgba(const uint8_t* pixels);
}

// The channel the interface starts and stops the texture through.
static const char* kChannelName = "scwc_preview";

// No texture is registered; a registrar's ids are its own, so zero cannot be the "none" value.
static const int64_t kNoTexture = -1;

// -------------------------------------------------------------------------------------------
// The texture
// -------------------------------------------------------------------------------------------

// The engine's pixel buffer, pinned to the newest frame of the capture path's preview.
//
// A frame is released at the start of the next call, which is the next tick of the render thread:
// the engine has taken the pixels it was handed by then, so the buffer it painted from lives exactly
// as long as the engine may paint it.
typedef struct _ScwcPreviewTexture ScwcPreviewTexture;

struct _ScwcPreviewTexture {
  FlPixelBufferTexture parent_instance;

  // The frame handed to the engine, or null while it paints none.
  const uint8_t* pixels;

  // The shape of that frame. Written by the engine's copy on the render thread, read by `size` on
  // the platform thread, so it is asked for atomically rather than through a frame the plugin would
  // have to take — and taking one would be a live preview frame thrown away for a number.
  std::atomic<uint32_t> frame_width;
  std::atomic<uint32_t> frame_height;
};

typedef struct {
  FlPixelBufferTextureClass parent_class;
} ScwcPreviewTextureClass;

G_DEFINE_TYPE(ScwcPreviewTexture, scwc_preview_texture,
              fl_pixel_buffer_texture_get_type())

#define SCWC_PREVIEW_TEXTURE(obj)                                \
  (G_TYPE_CHECK_INSTANCE_CAST((obj), scwc_preview_texture_get_type(), \
                              ScwcPreviewTexture))

// Hands the engine the newest frame the capture path published.
static gboolean scwc_preview_texture_copy_pixels(FlPixelBufferTexture* texture,
                                                const uint8_t** buffer,
                                                uint32_t* width,
                                                uint32_t* height,
                                                GError** error) {
  (void)error;
  ScwcPreviewTexture* self = SCWC_PREVIEW_TEXTURE(texture);

  // The engine asked again, so it has taken what the last call handed it: let go of that frame
  // before asking the capture path for its successor.
  if (self->pixels != nullptr) {
    scwc_preview_release_rgba(self->pixels);
    self->pixels = nullptr;
  }

  uint32_t frame_width = 0;
  uint32_t frame_height = 0;
  const uint8_t* pixels = scwc_preview_latest_rgba(&frame_width, &frame_height);
  if (pixels == nullptr) {
    // Nothing has been captured yet, or the capture path is not publishing for a texture yet. The
    // engine asks again when the path says a frame is waiting.
    return FALSE;
  }

  self->pixels = pixels;
  self->frame_width.store(frame_width);
  self->frame_height.store(frame_height);

  *buffer = pixels;
  *width = frame_width;
  *height = frame_height;
  return TRUE;
}

static void scwc_preview_texture_finalize(GObject* object) {
  ScwcPreviewTexture* self = SCWC_PREVIEW_TEXTURE(object);
  if (self->pixels != nullptr) {
    scwc_preview_release_rgba(self->pixels);
    self->pixels = nullptr;
  }
  G_OBJECT_CLASS(scwc_preview_texture_parent_class)->finalize(object);
}

static void scwc_preview_texture_class_init(ScwcPreviewTextureClass* klass) {
  G_OBJECT_CLASS(klass)->finalize = scwc_preview_texture_finalize;
  FL_PIXEL_BUFFER_TEXTURE_CLASS(klass)->copy_pixels =
      scwc_preview_texture_copy_pixels;
}

static void scwc_preview_texture_init(ScwcPreviewTexture* self) {
  self->pixels = nullptr;
  self->frame_width.store(0);
  self->frame_height.store(0);
}

// -------------------------------------------------------------------------------------------
// The plugin
// -------------------------------------------------------------------------------------------

// The `scwc_preview` channel: it owns the texture's lifetime and answers the interface.
struct _ScwcPreviewPlugin {
  GObject parent_instance;

  // The engine's texture registry, which owns the registered texture's id.
  FlTextureRegistrar* textures;

  // The registered texture, or null while none is.
  ScwcPreviewTexture* texture;
};

G_DEFINE_TYPE(ScwcPreviewPlugin, scwc_preview_plugin, g_object_get_type())

#define SCWC_PREVIEW_PLUGIN(obj)                                \
  (G_TYPE_CHECK_INSTANCE_CAST((obj), scwc_preview_plugin_get_type(), \
                              ScwcPreviewPlugin))

// The plugin the capture path notifies, cleared when it is disposed of.
//
// One registrar means one plugin. The notice arrives on the capture thread, so the pointer is weak
// — a plugin being torn down on the platform thread cannot leave it dangling — and it is only looked
// at on the platform thread, where the notice is handed over.
static ScwcPreviewPlugin* g_plugin = nullptr;

// Tells the engine that the texture has a newer frame to paint, on the platform thread.
static gboolean scwc_preview_plugin_frame_available(gpointer user_data) {
  (void)user_data;
  if (g_plugin == nullptr || g_plugin->texture == nullptr) {
    return G_SOURCE_REMOVE;
  }
  fl_texture_registrar_mark_texture_frame_available(g_plugin->textures,
                                                   FL_TEXTURE(g_plugin->texture));
  return G_SOURCE_REMOVE;
}

// The capture path's notice that a newer frame is waiting.
//
// It runs on the capture thread, which must not wait for the platform thread: the notice goes to the
// main loop, where the texture and the engine live, and this returns.
static void scwc_preview_plugin_frame_ready(void) {
  g_idle_add(scwc_preview_plugin_frame_available, nullptr);
}

// Registers a texture and starts the capture path publishing into it, answering its id, or
// `kNoTexture` when the engine refused the texture.
static int64_t scwc_preview_plugin_start_texture(ScwcPreviewPlugin* self) {
  if (self->texture != nullptr) {
    // An interface that starts twice means the texture it already has.
    return fl_texture_get_id(FL_TEXTURE(self->texture));
  }
  ScwcPreviewTexture* texture =
      SCWC_PREVIEW_TEXTURE(g_object_new(scwc_preview_texture_get_type(), nullptr));
  if (!fl_texture_registrar_register_texture(self->textures, FL_TEXTURE(texture))) {
    g_object_unref(texture);
    return kNoTexture;
  }
  self->texture = texture;
  // A texture is a consumer: the capture path scales its next frames to the size the interface asked
  // for and offers them here.
  scwc_preview_register_rgba_callback(scwc_preview_plugin_frame_ready);
  return fl_texture_get_id(FL_TEXTURE(texture));
}

// Unregisters the texture and stops the capture path.
static void scwc_preview_plugin_stop_texture(ScwcPreviewPlugin* self) {
  if (self->texture == nullptr) {
    return;
  }
  // Out of the engine's hands first, so nothing paints from a frame that is about to go.
  fl_texture_registrar_unregister_texture(self->textures, FL_TEXTURE(self->texture));
  g_object_unref(self->texture);
  self->texture = nullptr;
  scwc_preview_register_rgba_callback(nullptr);
}

// The shape of the frame the texture paints, or null while it has painted none.
static FlValue* scwc_preview_plugin_frame_size(ScwcPreviewPlugin* self) {
  if (self->texture == nullptr) {
    return fl_value_new_null();
  }
  const uint32_t width = self->texture->frame_width.load();
  const uint32_t height = self->texture->frame_height.load();
  if (width == 0 || height == 0) {
    return fl_value_new_null();
  }
  FlValue* size = fl_value_new_list();
  fl_value_append_take(size, fl_value_new_int(width));
  fl_value_append_take(size, fl_value_new_int(height));
  return size;
}

// Serves one call from the interface.
static void scwc_preview_plugin_handle_method_call(ScwcPreviewPlugin* self,
                                                  FlMethodCall* method_call) {
  g_autoptr(FlMethodResponse) response = nullptr;
  const gchar* method = fl_method_call_get_name(method_call);
  if (g_strcmp0(method, "start") == 0) {
    const int64_t texture_id = scwc_preview_plugin_start_texture(self);
    if (texture_id == kNoTexture) {
      response = FL_METHOD_RESPONSE(fl_method_error_response_new(
          "no-texture", "the engine has no room for another texture", nullptr));
    } else {
      response = FL_METHOD_RESPONSE(
          fl_method_success_response_new(fl_value_new_int(texture_id)));
    }
  } else if (g_strcmp0(method, "stop") == 0) {
    scwc_preview_plugin_stop_texture(self);
    response = FL_METHOD_RESPONSE(fl_method_success_response_new(nullptr));
  } else if (g_strcmp0(method, "size") == 0) {
    response = FL_METHOD_RESPONSE(
        fl_method_success_response_new(scwc_preview_plugin_frame_size(self)));
  } else {
    response = FL_METHOD_RESPONSE(fl_method_not_implemented_response_new());
  }
  fl_method_call_respond(method_call, response, nullptr);
}

static void scwc_preview_plugin_method_call(FlMethodChannel* channel,
                                           FlMethodCall* method_call,
                                           gpointer user_data) {
  (void)channel;
  scwc_preview_plugin_handle_method_call(SCWC_PREVIEW_PLUGIN(user_data), method_call);
}

static void scwc_preview_plugin_dispose(GObject* object) {
  ScwcPreviewPlugin* self = SCWC_PREVIEW_PLUGIN(object);
  scwc_preview_plugin_stop_texture(self);
  if (self->textures != nullptr) {
    g_object_unref(self->textures);
    self->textures = nullptr;
  }
  G_OBJECT_CLASS(scwc_preview_plugin_parent_class)->dispose(object);
}

static void scwc_preview_plugin_class_init(ScwcPreviewPluginClass* klass) {
  G_OBJECT_CLASS(klass)->dispose = scwc_preview_plugin_dispose;
}

static void scwc_preview_plugin_init(ScwcPreviewPlugin* self) {
  self->textures = nullptr;
  self->texture = nullptr;
}

void scwc_preview_plugin_register_with_registrar(FlPluginRegistrar* registrar) {
  ScwcPreviewPlugin* plugin =
      SCWC_PREVIEW_PLUGIN(g_object_new(scwc_preview_plugin_get_type(), nullptr));
  g_object_add_weak_pointer(G_OBJECT(plugin), reinterpret_cast<gpointer*>(&g_plugin));
  plugin->textures = FL_TEXTURE_REGISTRAR(
      g_object_ref(fl_plugin_registrar_get_texture_registrar(registrar)));

  g_autoptr(FlStandardMethodCodec) codec = fl_standard_method_codec_new();
  g_autoptr(FlMethodChannel) channel = fl_method_channel_new(
      fl_plugin_registrar_get_messenger(registrar), kChannelName, FL_METHOD_CODEC(codec));
  fl_method_channel_set_method_call_handler(channel, scwc_preview_plugin_method_call,
                                            g_object_ref(plugin), g_object_unref);
}
